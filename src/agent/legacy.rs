#![cfg(test)]

#[path = "context.rs"]
mod context;
#[path = "runner.rs"]
mod runner;

pub(crate) use context::{
    TimestampSource, TurnContext, TurnContextDependencies, TurnContextError,
    system_timestamp_source,
};
pub(crate) use runner::{
    MAX_RUNNER_EVENT_CAPACITY, RunnerEvent, RunnerEventSendError, RunnerEventSink, TurnFailure,
    TurnTaskResult, run_turn,
};

const _: () = {
    let _ = std::mem::size_of::<TimestampSource>();
    let _ = std::mem::size_of::<TurnContext>();
    let _ = std::mem::size_of::<TurnContextDependencies>();
    let _ = std::mem::size_of::<TurnContextError>();
    let _ = system_timestamp_source;
    let _ = std::mem::size_of::<RunnerEvent>();
    let _ = std::mem::size_of::<RunnerEventSendError>();
    let _ = std::mem::size_of::<RunnerEventSink>();
    let _ = std::mem::size_of::<TurnFailure>();
    let _ = std::mem::size_of::<TurnTaskResult>();
    let _ = MAX_RUNNER_EVENT_CAPACITY;
    let _ = RunnerEventSink::channel;
    let _ = RunnerEventSink::try_publish_model;
    let _ = RunnerEventSink::try_publish_tool;
    let _ = run_turn;
};

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, VecDeque};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::context::TurnContext;
    use super::context::{TimestampSource, TurnContextDependencies, system_timestamp_source};
    use super::runner::{
        MAX_RUNNER_EVENT_CAPACITY, RunnerEvent, RunnerEventSendError, RunnerEventSink,
    };
    use super::runner::{TurnTaskResult, run_turn};
    use crate::config::RetryPolicy;
    use crate::ids::{SessionId, TurnId};
    use crate::model::legacy_provider::{LegacyModelFuture, LegacyModelProvider};
    use crate::model::legacy_registry::LegacyProviderRegistry;
    use crate::model::{
        AssistantPart, LegacyModelCallContext, LegacyModelDescriptor, LegacyModelEvent,
        LegacyModelEventSink, LegacyModelGateway, LegacyModelSelection, LegacyProviderId,
        ModelError, ModelFinishReason, ModelLimits, ModelRequest, ModelResponse,
        ReasoningPreference, Usage,
    };
    use crate::prompt::CompactionConfig;
    use crate::storage::conversation::{ConversationLog, NewConversationEntry};
    use crate::storage::store::{
        SessionStore, StoredCompactionConfig, StoredExecutionConfig, StoredModelConfig,
        StoredSessionConfig,
    };
    use crate::time::{Timestamp, TimestampError};
    use crate::tools::legacy_policy::{
        LegacyAllowConfiguredTools, LegacyToolContextView, LegacyToolDecision, LegacyToolPolicy,
        LegacyToolRequest,
    };
    use crate::tools::registry::{LegacyToolFuture, ToolRegistry};
    use crate::tools::{
        InteractionClient, InteractionReceiver, LegacyTool, LegacyToolContext, LegacyToolError,
        LegacyToolOutput, ToolName, ToolSpec,
    };
    use crate::workspace::Workspace;
    use crate::workspace::root::WorkspaceAccess;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    static CANCEL_ON_TIMESTAMP_TOKEN: Mutex<Option<CancellationToken>> = Mutex::new(None);
    static TIMESTAMP_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct ScriptedProvider {
        id: LegacyProviderId,
        models: Vec<LegacyModelDescriptor>,
        steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
        calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    enum ScriptedStep {
        Response {
            response: ModelResponse,
            event: Option<LegacyModelEvent>,
        },
        CancelResponse {
            response: ModelResponse,
            event: Option<LegacyModelEvent>,
        },
        EventThenPending {
            event: LegacyModelEvent,
        },
        MutateBeforeResponse {
            response: ModelResponse,
            log: Arc<ConversationLog>,
            turn_id: TurnId,
            text: String,
        },
        ExposeLateSink {
            response: ModelResponse,
            slot: Arc<Mutex<Option<LegacyModelEventSink>>>,
        },
        Error(ModelError),
        ErrorWithEvent {
            error: ModelError,
            event: LegacyModelEvent,
        },
        Pending,
    }

    impl LegacyModelProvider for ScriptedProvider {
        fn id(&self) -> &LegacyProviderId {
            &self.id
        }

        fn models(&self) -> &[LegacyModelDescriptor] {
            &self.models
        }

        fn generate(
            &self,
            request: ModelRequest,
            ctx: LegacyModelCallContext,
        ) -> LegacyModelFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(request);
            let step = self
                .steps
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
                .unwrap_or(ScriptedStep::Pending);
            Box::pin(async move {
                match step {
                    ScriptedStep::Response { response, event } => {
                        if let Some(event) = event {
                            let _ = ctx.publish(event);
                        }
                        Ok(response)
                    }
                    ScriptedStep::CancelResponse { response, event } => {
                        if let Some(event) = event {
                            let _ = ctx.publish(event);
                        }
                        ctx.cancellation().cancel();
                        Ok(response)
                    }
                    ScriptedStep::EventThenPending { event } => {
                        let _ = ctx.publish(event);
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                    ScriptedStep::MutateBeforeResponse {
                        response,
                        log,
                        turn_id,
                        text,
                    } => {
                        log.append(NewConversationEntry::User {
                            turn_id,
                            timestamp: timestamp(),
                            text,
                        })
                        .await
                        .unwrap();
                        Ok(response)
                    }
                    ScriptedStep::ExposeLateSink { response, slot } => {
                        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(ctx.event_sink().clone());
                        Ok(response)
                    }
                    ScriptedStep::Error(error) => Err(error),
                    ScriptedStep::ErrorWithEvent { error, event } => {
                        let _ = ctx.publish(event);
                        Err(error)
                    }
                    ScriptedStep::Pending => {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            })
        }
    }

    struct Harness {
        store: SessionStore,
        log: Arc<ConversationLog>,
        workspace: Arc<Workspace>,
        root: std::path::PathBuf,
        events: mpsc::Receiver<RunnerEvent>,
        interactions: Option<InteractionReceiver>,
        steps: Arc<Mutex<VecDeque<ScriptedStep>>>,
        calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Harness {
        async fn cleanup(self) {
            self.log.close().await.unwrap();
            self.workspace.shutdown().await.unwrap();
            self.store.shutdown().await.unwrap();
            fs::remove_dir_all(self.root).unwrap();
        }
    }

    struct HarnessOptions {
        registry: ToolRegistry,
        policy: Arc<dyn LegacyToolPolicy>,
        enabled_tools: BTreeSet<ToolName>,
        max_tool_rounds: u8,
        cancellation: CancellationToken,
        compaction: CompactionConfig,
        timestamp_source: TimestampSource,
        retry_policy: RetryPolicy,
        event_capacity: usize,
        request_limits: ModelLimits,
        descriptor_limits: ModelLimits,
        supported_reasoning: BTreeSet<ReasoningPreference>,
        reasoning: ReasoningPreference,
    }

    impl HarnessOptions {
        fn default() -> Self {
            Self {
                registry: ToolRegistry::builder().build(),
                policy: Arc::new(LegacyAllowConfiguredTools::new()),
                enabled_tools: BTreeSet::new(),
                max_tool_rounds: 4,
                cancellation: CancellationToken::new(),
                compaction: CompactionConfig::new(1_000_000, 999_999).unwrap(),
                timestamp_source: fixed_timestamp_source,
                retry_policy: RetryPolicy::new(1, Duration::ZERO).unwrap(),
                event_capacity: 16,
                request_limits: ModelLimits::default(),
                descriptor_limits: ModelLimits::default(),
                supported_reasoning: BTreeSet::from([
                    ReasoningPreference::Auto,
                    ReasoningPreference::Disabled,
                    ReasoningPreference::Low,
                    ReasoningPreference::Medium,
                    ReasoningPreference::High,
                ]),
                reasoning: ReasoningPreference::Auto,
            }
        }
    }

    async fn harness(steps: Vec<ScriptedStep>) -> (TurnContext, Harness) {
        harness_with(steps, HarnessOptions::default()).await
    }

    async fn harness_with(
        steps: Vec<ScriptedStep>,
        options: HarnessOptions,
    ) -> (TurnContext, Harness) {
        harness_with_result(steps, options).await.unwrap()
    }

    async fn harness_with_result(
        steps: Vec<ScriptedStep>,
        options: HarnessOptions,
    ) -> Result<(TurnContext, Harness), super::context::TurnContextError> {
        let session_id = SessionId::new().unwrap();
        let root = std::env::temp_dir().join(format!("minicore-p6-agent-{session_id}"));
        let workspace_root = root.join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let model_selection =
            LegacyModelSelection::new("scripted".parse().unwrap(), "model".parse().unwrap());
        let config = StoredSessionConfig::new(
            session_id,
            timestamp(),
            timestamp(),
            workspace_root.clone(),
            StoredModelConfig::new(model_selection.clone()),
            "system".to_owned(),
            StoredExecutionConfig::new(
                BTreeSet::new(),
                StoredCompactionConfig::new(1_000_000, 999_999).unwrap(),
                4,
            )
            .unwrap(),
        )
        .unwrap();
        store.create(&config).await.unwrap();
        let log = Arc::new(ConversationLog::open(&store, session_id).await.unwrap());
        let turn_id = TurnId::new().unwrap();
        log.append(NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: "question".to_owned(),
        })
        .await
        .unwrap();

        let request_limits = options.request_limits;
        let descriptor = LegacyModelDescriptor::new(
            model_selection.clone(),
            "scripted-model",
            options.descriptor_limits,
            options.supported_reasoning,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let steps = Arc::new(Mutex::new(steps.into_iter().collect()));
        let provider = ScriptedProvider {
            id: "scripted".parse().unwrap(),
            models: vec![descriptor],
            steps: Arc::clone(&steps),
            calls: Arc::clone(&calls),
            requests: Arc::clone(&requests),
        };
        let mut providers = LegacyProviderRegistry::builder();
        providers.register(provider).unwrap();
        let gateway = LegacyModelGateway::new(providers.build());
        let workspace =
            Arc::new(Workspace::open(&workspace_root, WorkspaceAccess::ReadWrite).unwrap());
        let (events, event_receiver) = RunnerEventSink::channel(options.event_capacity).unwrap();
        let (interactions, interaction_receiver) = InteractionClient::channel();
        let context = match TurnContext::new(
            session_id,
            turn_id,
            options.enabled_tools.clone(),
            options.max_tool_rounds,
            TurnContextDependencies {
                prompt_builder: crate::prompt::PromptBuilder::new("system", "coding").unwrap(),
                prompt_options: crate::prompt::PromptBuildOptions::new(
                    model_selection,
                    request_limits,
                    options.reasoning,
                ),
                compactor: crate::prompt::Compactor::new(options.compaction),
                gateway,
                tools: options.registry,
                policy: options.policy,
                workspace: Arc::clone(&workspace),
                conversation: Arc::clone(&log),
                interactions,
                cancellation: options.cancellation.clone(),
                timestamp_source: options.timestamp_source,
                retry_policy: options.retry_policy,
                events,
            },
        ) {
            Ok(context) => context,
            Err(error) => {
                let _ = log.close().await;
                let _ = workspace.shutdown().await;
                let _ = store.shutdown().await;
                let _ = fs::remove_dir_all(root);
                return Err(error);
            }
        };
        Ok((
            context,
            Harness {
                store,
                log,
                workspace,
                root,
                events: event_receiver,
                interactions: Some(interaction_receiver),
                steps,
                calls,
                requests,
            },
        ))
    }

    fn timestamp() -> Timestamp {
        "2026-08-19T12:34:56.789Z".parse().unwrap()
    }

    fn fixed_timestamp_source() -> Result<Timestamp, TimestampError> {
        Ok(timestamp())
    }

    fn failing_timestamp_source() -> Result<Timestamp, TimestampError> {
        Err(TimestampError::Invalid)
    }

    fn cancel_on_third_timestamp() -> Result<Timestamp, TimestampError> {
        if TIMESTAMP_CALLS.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            if let Some(token) = CANCEL_ON_TIMESTAMP_TOKEN
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
            {
                token.cancel();
            }
        }
        Ok(timestamp())
    }

    enum ToolBehavior {
        Success(String),
        Failure(String),
        Error,
        PanicSync,
        PanicFuture,
        CancelThenSuccess(CancellationToken),
    }

    struct TestTool {
        spec: ToolSpec,
        behavior: ToolBehavior,
        order: Arc<Mutex<Vec<String>>>,
    }

    impl TestTool {
        fn new(name: &str, behavior: ToolBehavior, order: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                spec: ToolSpec::new(
                    name.parse().unwrap(),
                    "test tool",
                    json!({"type": "object"}),
                )
                .unwrap(),
                behavior,
                order,
            }
        }
    }

    impl LegacyTool for TestTool {
        fn spec(&self) -> ToolSpec {
            self.spec.clone()
        }

        fn execute<'a>(
            &'a self,
            _ctx: LegacyToolContext<'a>,
            _args: Value,
        ) -> LegacyToolFuture<'a> {
            self.order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(self.spec.name().as_str().to_owned());
            if matches!(&self.behavior, ToolBehavior::PanicSync) {
                panic!("test tool panic");
            }
            if matches!(&self.behavior, ToolBehavior::PanicFuture) {
                return Box::pin(async { panic!("test tool future panic") });
            }
            match &self.behavior {
                ToolBehavior::Success(text) => {
                    let output = LegacyToolOutput::success(text.clone()).unwrap();
                    Box::pin(async move { Ok(output) })
                }
                ToolBehavior::Failure(text) => {
                    let output = LegacyToolOutput::failure(text.clone()).unwrap();
                    Box::pin(async move { Ok(output) })
                }
                ToolBehavior::Error => Box::pin(async { Err(LegacyToolError::Internal) }),
                ToolBehavior::CancelThenSuccess(token) => {
                    let token = token.clone();
                    let output =
                        LegacyToolOutput::success("completed before cancellation").unwrap();
                    Box::pin(async move {
                        token.cancel();
                        Ok(output)
                    })
                }
                ToolBehavior::PanicSync | ToolBehavior::PanicFuture => unreachable!(),
            }
        }
    }

    fn tool_registry(
        tools: Vec<TestTool>,
    ) -> (ToolRegistry, BTreeSet<ToolName>, Arc<Mutex<Vec<String>>>) {
        let order = Arc::clone(&tools[0].order);
        let enabled = tools
            .iter()
            .map(|tool| tool.spec.name().clone())
            .collect::<BTreeSet<_>>();
        let mut builder = ToolRegistry::builder();
        for tool in tools {
            builder.register(tool).unwrap();
        }
        (builder.build(), enabled, order)
    }

    fn tool_call(index: u32, name: &str) -> crate::model::ToolCall {
        crate::model::ToolCall::new(
            crate::ids::ToolCallId::new(format!("call-{index}")).unwrap(),
            name.parse().unwrap(),
            json!({}),
            index,
        )
        .unwrap()
    }

    fn tool_response(names: &[&str]) -> ModelResponse {
        ModelResponse::new(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| AssistantPart::ToolCall(tool_call(index as u32, name)))
                .collect(),
            ModelFinishReason::ToolCalls,
            None,
        )
        .unwrap()
    }

    fn text_response(text: &str) -> ModelResponse {
        ModelResponse::new(
            vec![AssistantPart::Text(text.to_owned())],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap()
    }

    struct FixedPolicy {
        decision: LegacyToolDecision,
    }

    impl LegacyToolPolicy for FixedPolicy {
        fn decide(
            &self,
            _request: &LegacyToolRequest<'_>,
            _ctx: &LegacyToolContextView<'_>,
        ) -> LegacyToolDecision {
            self.decision.clone()
        }
    }

    struct PanickingPolicy;

    impl LegacyToolPolicy for PanickingPolicy {
        fn decide(
            &self,
            _request: &LegacyToolRequest<'_>,
            _ctx: &LegacyToolContextView<'_>,
        ) -> LegacyToolDecision {
            panic!("test policy panic");
        }
    }

    #[test]
    fn timestamp_sources_are_system_and_fixed() {
        let system: TimestampSource = system_timestamp_source();
        let now = system().unwrap();
        assert_eq!(now.as_str().len(), 24);
        let fixed_timestamp: Timestamp = "2026-08-19T12:34:56.789Z".parse().unwrap();
        let fixed: TimestampSource = fixed_timestamp_source;
        assert_eq!(fixed().unwrap(), fixed_timestamp);
        assert_eq!(fixed().unwrap(), fixed_timestamp);
    }

    #[test]
    fn ordinary_retry_keeps_one_request_arc_until_gateway_boundary() {
        let source = include_str!("runner.rs");
        assert!(source.contains("request: Arc<crate::model::ModelRequest>"));
        assert!(source.contains("Arc::clone(&request)"));
        assert!(source.contains("(*request).clone()"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_context_rejects_zero_and_more_than_64_tool_rounds() {
        let mut zero = HarnessOptions::default();
        zero.max_tool_rounds = 0;
        assert!(matches!(
            harness_with_result(Vec::new(), zero).await,
            Err(super::context::TurnContextError::InvalidToolRounds)
        ));
        let mut too_many = HarnessOptions::default();
        too_many.max_tool_rounds = 65;
        assert!(matches!(
            harness_with_result(Vec::new(), too_many).await,
            Err(super::context::TurnContextError::InvalidToolRounds)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_context_requires_active_and_disabled_reasoning_support() {
        let mut missing_disabled = HarnessOptions::default();
        missing_disabled.supported_reasoning = BTreeSet::from([ReasoningPreference::Auto]);
        assert!(matches!(
            harness_with_result(Vec::new(), missing_disabled).await,
            Err(super::context::TurnContextError::InvalidModelConfiguration)
        ));

        let mut missing_active = HarnessOptions::default();
        missing_active.supported_reasoning = BTreeSet::from([ReasoningPreference::Disabled]);
        assert!(matches!(
            harness_with_result(Vec::new(), missing_active).await,
            Err(super::context::TurnContextError::InvalidModelConfiguration)
        ));
    }

    #[test]
    fn runner_event_sink_is_bounded_and_deltas_are_best_effort() {
        let (sink, mut receiver) = RunnerEventSink::channel(1).unwrap();
        let event = RunnerEvent::Model(crate::model::LegacyModelEvent::TextDelta {
            delta: "delta".to_owned(),
        });
        assert!(sink.try_publish_model(event.clone()));
        assert!(!sink.try_publish_model(event.clone()));
        assert_eq!(receiver.try_recv().unwrap(), event);
        assert!(matches!(
            sink.try_publish_tool(RunnerEvent::Model(
                crate::model::LegacyModelEvent::ReasoningDelta {
                    delta: "late".to_owned(),
                }
            )),
            Err(RunnerEventSendError::InvalidEvent)
        ));
        let call = crate::tools::LegacyToolCallSummary::new(
            crate::ids::ToolCallId::new("call").unwrap(),
            "tool".parse().unwrap(),
            0,
        )
        .unwrap();
        let (sink, _receiver) = RunnerEventSink::channel(1).unwrap();
        sink.try_publish_tool(RunnerEvent::ToolStarted(call.clone()))
            .unwrap();
        assert_eq!(
            sink.try_publish_tool(RunnerEvent::ToolStarted(call)),
            Err(RunnerEventSendError::Full)
        );
    }

    #[test]
    fn runner_event_sink_rejects_invalid_capacity_and_observes_receiver_close() {
        assert!(matches!(
            RunnerEventSink::channel(0),
            Err(RunnerEventSendError::InvalidEvent)
        ));
        assert!(matches!(
            RunnerEventSink::channel(MAX_RUNNER_EVENT_CAPACITY + 1),
            Err(RunnerEventSendError::InvalidEvent)
        ));
        assert!(matches!(
            RunnerEventSink::channel(usize::MAX),
            Err(RunnerEventSendError::InvalidEvent)
        ));
        let (sink, receiver) = RunnerEventSink::channel(MAX_RUNNER_EVENT_CAPACITY).unwrap();
        drop(receiver);
        assert_eq!(
            sink.try_publish_tool(RunnerEvent::ToolFinished(
                crate::tools::LegacyToolResultSummary::new(
                    crate::ids::ToolCallId::new("call").unwrap(),
                    crate::tools::LegacyToolResultStatus::Failed,
                )
                .unwrap(),
            )),
            Err(RunnerEventSendError::Closed)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_only_turn_appends_assistant_forwards_delta_and_reports_usage() {
        let response = ModelResponse::new(
            vec![AssistantPart::Text("answer".to_owned())],
            ModelFinishReason::Stop,
            Some(Usage::new(3, 5, 1)),
        )
        .unwrap();
        let (context, mut harness) = harness(vec![ScriptedStep::Response {
            response,
            event: Some(LegacyModelEvent::TextDelta {
                delta: "answer".to_owned(),
            }),
        }])
        .await;
        let result = run_turn(context).await;
        let usage = match result {
            TurnTaskResult::Completed { usage }
            | TurnTaskResult::Cancelled { usage }
            | TurnTaskResult::Failed { usage, .. } => usage,
        };
        assert_eq!(usage.input_tokens(), Some(3));
        assert_eq!(usage.output_tokens(), Some(5));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            harness.events.recv().await,
            Some(RunnerEvent::Model(LegacyModelEvent::TextDelta { delta })) if delta == "answer"
        ));
        assert!(harness.events.recv().await.is_none());
        let snapshot = harness.log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 2);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn late_model_events_are_rejected_after_attempt_close() {
        let slot = Arc::new(Mutex::new(None));
        let (context, mut harness) = harness(vec![ScriptedStep::ExposeLateSink {
            response: text_response("done"),
            slot: Arc::clone(&slot),
        }])
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        let late = slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap();
        assert!(!late.publish(LegacyModelEvent::TextDelta {
            delta: "late".to_owned(),
        }));
        assert!(harness.events.try_recv().is_err());
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_and_multiple_tool_rounds_execute_in_call_index_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, order) = tool_registry(vec![
            TestTool::new(
                "alpha",
                ToolBehavior::Success("a".to_owned()),
                Arc::clone(&order),
            ),
            TestTool::new(
                "beta",
                ToolBehavior::Success("b".to_owned()),
                Arc::clone(&order),
            ),
        ]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        let (context, mut harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha", "beta"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: tool_response(&["beta"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("done"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert_eq!(
            context
                .tool_specs()
                .iter()
                .map(|spec| spec.name().as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        let result = run_turn(context).await;
        assert!(matches!(result, TurnTaskResult::Completed { .. }));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha", "beta", "beta"]
        );
        assert_eq!(harness.calls.load(Ordering::SeqCst), 3);
        let snapshot = harness.log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 7);
        let mut events = Vec::new();
        while let Ok(event) = harness.events.try_recv() {
            events.push(event);
        }
        assert!(matches!(events[0], RunnerEvent::ToolStarted(_)));
        assert!(matches!(events[1], RunnerEvent::ToolFinished(_)));
        assert!(matches!(events[2], RunnerEvent::ToolStarted(_)));
        assert!(matches!(events[3], RunnerEvent::ToolFinished(_)));
        assert!(matches!(events[4], RunnerEvent::ToolStarted(_)));
        assert!(matches!(events[5], RunnerEvent::ToolFinished(_)));
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deny_and_ask_policy_settle_truthful_tool_results() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("executed".to_owned()),
            Arc::clone(&order),
        )]);
        let mut deny_options = HarnessOptions::default();
        deny_options.registry = registry.clone();
        deny_options.enabled_tools = enabled.clone();
        deny_options.policy = Arc::new(FixedPolicy {
            decision: LegacyToolDecision::deny("policy denied").unwrap(),
        });
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after deny"),
                    event: None,
                },
            ],
            deny_options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        let snapshot = harness.log.snapshot().await;
        let denied = serde_json::to_value(&*snapshot.entries()[2]).unwrap();
        assert_eq!(denied["result"]["is_error"], true);
        assert_eq!(denied["result"]["text"], "policy denied");
        harness.cleanup().await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("approved".to_owned()),
            Arc::clone(&order),
        )]);
        let mut ask_options = HarnessOptions::default();
        ask_options.registry = registry;
        ask_options.enabled_tools = enabled;
        ask_options.policy = Arc::new(FixedPolicy {
            decision: LegacyToolDecision::ask(
                "approve alpha",
                Some(vec!["yes".to_owned(), "no".to_owned()]),
            )
            .unwrap(),
        });
        let (context, mut harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after ask"),
                    event: None,
                },
            ],
            ask_options,
        )
        .await;
        let mut task = Box::pin(run_turn(context));
        let request = tokio::select! {
            result = &mut task => panic!("turn completed before approval: {result:?}"),
            request = harness.interactions.as_mut().unwrap().recv() => request.unwrap(),
        };
        assert_eq!(request.question(), "approve alpha");
        assert_eq!(
            request.choices(),
            Some(["yes".to_owned(), "no".to_owned()].as_slice())
        );
        request
            .respond(crate::tools::LegacyUserAnswer::new("ALLOW").unwrap())
            .unwrap();
        assert!(matches!(task.await, TurnTaskResult::Completed { .. }));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        let snapshot = harness.log.snapshot().await;
        let approved = serde_json::to_value(&*snapshot.entries()[2]).unwrap();
        assert_eq!(approved["result"]["is_error"], false);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_panics_and_errors_become_redacted_failed_outputs() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![
            TestTool::new("alpha", ToolBehavior::Error, Arc::clone(&order)),
            TestTool::new("beta", ToolBehavior::PanicSync, Arc::clone(&order)),
            TestTool::new("gamma", ToolBehavior::PanicFuture, Arc::clone(&order)),
        ]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha", "beta", "gamma"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after failures"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha", "beta", "gamma"]
        );
        let snapshot = harness.log.snapshot().await;
        for entry in &snapshot.entries()[2..5] {
            let value = serde_json::to_value(&**entry).unwrap();
            assert_eq!(value["result"]["is_error"], true);
            assert_eq!(value["result"]["text"], "tool execution failed");
        }
        harness.cleanup().await;

        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("never".to_owned()),
            Arc::clone(&order),
        )]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        options.policy = Arc::new(PanickingPolicy);
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after policy panic"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        let snapshot = harness.log.snapshot().await;
        let value = serde_json::to_value(&*snapshot.entries()[2]).unwrap();
        assert_eq!(value["result"]["is_error"], true);
        assert_eq!(value["result"]["text"], "tool execution failed");
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn allowed_error_output_is_failed_and_finished_follows_persistence() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Failure("reported failure".to_owned()),
            Arc::clone(&order),
        )]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        let (context, mut harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("done"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        let first = harness.events.try_recv().unwrap();
        let second = harness.events.try_recv().unwrap();
        assert!(matches!(first, RunnerEvent::ToolStarted(_)));
        assert!(matches!(
            second,
            RunnerEvent::ToolFinished(ref summary)
                if summary.status() == crate::tools::LegacyToolResultStatus::Failed
        ));
        let snapshot = harness.log.snapshot().await;
        let result = serde_json::to_value(&*snapshot.entries()[2]).unwrap();
        assert_eq!(result["result"]["is_error"], true);
        assert_eq!(result["result"]["text"], "reported failure");
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disabled_tool_is_failed_without_started_event_or_execution() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, _enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("must not execute".to_owned()),
            Arc::clone(&order),
        )]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = BTreeSet::new();
        let (context, mut harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after disabled"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert!(matches!(
            harness.events.try_recv().unwrap(),
            RunnerEvent::ToolFinished(_)
        ));
        assert!(harness.events.try_recv().is_err());
        assert!(
            order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_tool_event_sink_is_lossy_without_changing_control_flow() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("full sink".to_owned()),
            Arc::clone(&order),
        )]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        options.event_capacity = 1;
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after full sink"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(harness.log.snapshot().await.entries().len(), 4);
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_round_limit_settles_every_call_and_keeps_prompt_valid() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![
            TestTool::new(
                "alpha",
                ToolBehavior::Success("a".to_owned()),
                Arc::clone(&order),
            ),
            TestTool::new(
                "beta",
                ToolBehavior::Success("b".to_owned()),
                Arc::clone(&order),
            ),
        ]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        options.max_tool_rounds = 1;
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: tool_response(&["alpha", "beta"]),
                    event: None,
                },
            ],
            options,
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::ToolRoundLimit,
                ..
            }
        ));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        let snapshot = harness.log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 6);
        for entry in &snapshot.entries()[4..6] {
            let value = serde_json::to_value(&**entry).unwrap();
            assert_eq!(value["result"]["text"], "tool round limit reached");
            assert_eq!(value["result"]["is_error"], true);
        }
        assert!(harness.log.prompt_view().await.is_ok());
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_admitted_tool_settles_remaining_calls_without_polling() {
        let cancellation = CancellationToken::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![
            TestTool::new(
                "alpha",
                ToolBehavior::CancelThenSuccess(cancellation.clone()),
                Arc::clone(&order),
            ),
            TestTool::new(
                "beta",
                ToolBehavior::Success("not executed".to_owned()),
                Arc::clone(&order),
            ),
        ]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        options.cancellation = cancellation.clone();
        let (context, harness) = harness_with(
            vec![ScriptedStep::Response {
                response: tool_response(&["alpha", "beta"]),
                event: None,
            }],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Cancelled { .. }
        ));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        let snapshot = harness.log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 4);
        let remaining = serde_json::to_value(&*snapshot.entries()[3]).unwrap();
        assert_eq!(remaining["result"]["text"], "cancelled");
        assert_eq!(remaining["result"]["is_error"], true);
        assert!(harness.log.prompt_view().await.is_ok());
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_tool_assistant_append_beats_round_limit_and_settles_calls() {
        let cancellation = CancellationToken::new();
        *CANCEL_ON_TIMESTAMP_TOKEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cancellation.clone());
        TIMESTAMP_CALLS.store(0, Ordering::SeqCst);
        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![
            TestTool::new(
                "alpha",
                ToolBehavior::Success("first".to_owned()),
                Arc::clone(&order),
            ),
            TestTool::new(
                "beta",
                ToolBehavior::Success("must not execute".to_owned()),
                Arc::clone(&order),
            ),
        ]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        options.max_tool_rounds = 1;
        options.cancellation = cancellation.clone();
        options.timestamp_source = cancel_on_third_timestamp;
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: tool_response(&["alpha"]),
                    event: None,
                },
                ScriptedStep::Response {
                    response: tool_response(&["beta"]),
                    event: None,
                },
            ],
            options,
        )
        .await;
        let result = run_turn(context).await;
        *CANCEL_ON_TIMESTAMP_TOKEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        assert!(matches!(result, TurnTaskResult::Cancelled { .. }));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        let snapshot = harness.log.snapshot().await;
        assert_eq!(snapshot.entries().len(), 5);
        let cancelled = serde_json::to_value(&*snapshot.entries()[4]).unwrap();
        assert_eq!(cancelled["result"]["text"], "cancelled");
        assert_ne!(cancelled["result"]["text"], "tool round limit reached");
        assert!(harness.log.prompt_view().await.is_ok());
        harness.cleanup().await;
    }

    async fn append_completed_turn(log: &ConversationLog, text: &str) -> TurnId {
        let turn_id = TurnId::new().unwrap();
        log.append(NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: text.to_owned(),
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::Assistant {
            turn_id,
            timestamp: timestamp(),
            text: Some(format!("answer {text}")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        })
        .await
        .unwrap();
        log.append(NewConversationEntry::TurnTerminal {
            turn_id,
            timestamp: timestamp(),
            outcome: crate::storage::conversation::StoredTurnOutcome::Completed,
        })
        .await
        .unwrap();
        turn_id
    }

    async fn append_current_user(log: &ConversationLog, text: &str) -> TurnId {
        let turn_id = TurnId::new().unwrap();
        log.append(NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: text.to_owned(),
        })
        .await
        .unwrap();
        turn_id
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_summarizes_once_preserves_current_turn_and_hides_summary_events() {
        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(100, 80).unwrap();
        let (context, mut harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: text_response("summary text"),
                    event: Some(LegacyModelEvent::ReasoningDelta {
                        delta: "summary must not escape".to_owned(),
                    }),
                },
                ScriptedStep::Response {
                    response: text_response("final answer"),
                    event: Some(LegacyModelEvent::TextDelta {
                        delta: "ordinary event".to_owned(),
                    }),
                },
            ],
            options,
        )
        .await;
        let _completed = append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        let result = run_turn(context).await;
        assert!(matches!(result, TurnTaskResult::Completed { .. }));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
        let prompt = harness.log.prompt_view().await.unwrap();
        assert!(prompt.messages().iter().any(|message| {
            matches!(message, crate::model::ModelMessage::User(text) if text == "question")
        }));
        let snapshot = harness.log.snapshot().await;
        assert!(
            snapshot
                .entries()
                .iter()
                .any(|entry| serde_json::to_value(&**entry).unwrap()["type"] == "summary")
        );
        assert!(matches!(
            harness.events.try_recv().unwrap(),
            RunnerEvent::Model(LegacyModelEvent::TextDelta { delta }) if delta == "ordinary event"
        ));
        assert!(harness.events.try_recv().is_err());
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_stale_replans_exactly_once_and_summary_failures_have_no_fallback() {
        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(100, 80).unwrap();
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Pending,
                ScriptedStep::Response {
                    response: text_response("summary after stale"),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("ordinary"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        let mutated_turn = TurnId::new().unwrap();
        harness
            .steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        harness
            .steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend([
                ScriptedStep::MutateBeforeResponse {
                    response: text_response("first summary"),
                    log: Arc::clone(&harness.log),
                    turn_id: mutated_turn,
                    text: "mutation".to_owned(),
                },
                ScriptedStep::Response {
                    response: text_response("second summary"),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("ordinary"),
                    event: None,
                },
            ]);
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            harness
                .log
                .snapshot()
                .await
                .entries()
                .iter()
                .filter(|entry| serde_json::to_value(&***entry).unwrap()["type"] == "summary")
                .count(),
            1
        );
        harness.cleanup().await;

        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(100, 80).unwrap();
        let (context, harness) = harness_with(
            vec![ScriptedStep::Error(ModelError::ProviderUnavailable)],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Model,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            harness
                .log
                .snapshot()
                .await
                .entries()
                .iter()
                .filter(|entry| serde_json::to_value(&***entry).unwrap()["type"] == "summary")
                .count(),
            0
        );
        harness.cleanup().await;

        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(100, 80).unwrap();
        let (context, harness) = harness_with(
            vec![ScriptedStep::Response {
                response: tool_response(&["missing"]),
                event: None,
            }],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Compaction,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            harness
                .log
                .snapshot()
                .await
                .entries()
                .iter()
                .filter(|entry| serde_json::to_value(&***entry).unwrap()["type"] == "summary")
                .count(),
            0
        );
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_context_overflow_forces_one_safe_compaction_recovery() {
        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(1_000_000, 80).unwrap();
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Error(ModelError::ContextOverflow),
                ScriptedStep::Response {
                    response: text_response("forced summary"),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("ordinary after recovery"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 3);
        assert_eq!(harness.log.snapshot().await.entries().len(), 7);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_prompt_context_overflow_uses_the_same_forced_compaction_path() {
        let mut options = HarnessOptions::default();
        options.request_limits = ModelLimits::new(Some(400), None).unwrap();
        options.descriptor_limits = ModelLimits::new(Some(1_000), None).unwrap();
        options.compaction = CompactionConfig::new(1_000_000, 400).unwrap();
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: text_response("local forced summary"),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("ordinary after local recovery"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(100)).await;
        append_current_user(&harness.log, &"current ".repeat(100)).await;
        let result = run_turn(context).await;
        assert!(matches!(result, TurnTaskResult::Completed { .. }));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_or_boundaryless_context_overflow_fails_compaction_without_fallback() {
        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(1_000_000, 80).unwrap();
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Error(ModelError::ContextOverflow),
                ScriptedStep::Response {
                    response: text_response("forced summary"),
                    event: None,
                },
                ScriptedStep::Error(ModelError::ContextOverflow),
            ],
            options,
        )
        .await;
        append_completed_turn(&harness.log, &"old ".repeat(300)).await;
        append_current_user(&harness.log, "question").await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Compaction,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            harness
                .log
                .snapshot()
                .await
                .entries()
                .iter()
                .filter(|entry| serde_json::to_value(&***entry).unwrap()["type"] == "summary")
                .count(),
            1
        );
        harness.cleanup().await;

        let mut options = HarnessOptions::default();
        options.compaction = CompactionConfig::new(1_000_000, 80).unwrap();
        let (context, harness) = harness_with(
            vec![ScriptedStep::Error(ModelError::ContextOverflow)],
            options,
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Compaction,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.log.snapshot().await.entries().len(), 1);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transient_503_delivery_retries_same_request_but_unsafe_error_does_not() {
        let transient = ModelError::detailed(
            crate::model::ModelErrorKind::ProviderUnavailable,
            crate::model::DeliveryState::NotStarted,
            true,
            None,
        )
        .unwrap();
        let mut options = HarnessOptions::default();
        options.retry_policy = RetryPolicy::new(2, Duration::ZERO).unwrap();
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Error(transient),
                ScriptedStep::Response {
                    response: text_response("retried"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
        {
            let requests = harness
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0], requests[1]);
        }
        harness.cleanup().await;

        let (context, harness) = harness_with(
            vec![ScriptedStep::Error(ModelError::ProviderUnavailable)],
            HarnessOptions::default(),
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Model,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        harness.cleanup().await;

        let mut options = HarnessOptions::default();
        options.retry_policy = RetryPolicy::new(2, Duration::ZERO).unwrap();
        let too_long_hint = ModelError::detailed(
            crate::model::ModelErrorKind::RateLimited,
            crate::model::DeliveryState::NotStarted,
            true,
            Some(Duration::from_secs(31)),
        )
        .unwrap();
        let (context, harness) =
            harness_with(vec![ScriptedStep::Error(too_long_hint)], options).await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Model,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        harness.cleanup().await;

        let mut options = HarnessOptions::default();
        options.retry_policy = RetryPolicy::new(2, Duration::ZERO).unwrap();
        let (context, mut harness) = harness_with(
            vec![ScriptedStep::ErrorWithEvent {
                error: transient,
                event: LegacyModelEvent::TextDelta {
                    delta: "provisional error event".to_owned(),
                },
            }],
            options,
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Model,
                ..
            }
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(harness.events.try_recv().is_err());
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_model_cancellation_is_prompt_and_successful_response_settles_first() {
        let cancellation = CancellationToken::new();
        let mut options = HarnessOptions::default();
        options.cancellation = cancellation.clone();
        let (context, mut harness) = harness_with(
            vec![ScriptedStep::EventThenPending {
                event: LegacyModelEvent::ReasoningDelta {
                    delta: "streaming before cancellation".to_owned(),
                },
            }],
            options,
        )
        .await;
        let mut task = Box::pin(run_turn(context));
        assert!(matches!(
            tokio::select! {
                result = &mut task => panic!("pending turn completed early: {result:?}"),
                event = harness.events.recv() => event,
            },
            Some(RunnerEvent::Model(LegacyModelEvent::ReasoningDelta { delta }))
                if delta == "streaming before cancellation"
        ));
        cancellation.cancel();
        assert!(matches!(task.await, TurnTaskResult::Cancelled { .. }));
        assert_eq!(harness.log.snapshot().await.entries().len(), 1);
        harness.cleanup().await;

        let options = HarnessOptions::default();
        let cancellation = options.cancellation.clone();
        let (context, harness) = harness_with(
            vec![ScriptedStep::CancelResponse {
                response: text_response("returned"),
                event: None,
            }],
            options,
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(result, TurnTaskResult::Cancelled { .. }));
        assert!(cancellation.is_cancelled());
        assert_eq!(harness.log.snapshot().await.entries().len(), 1);
        harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timestamp_and_conversation_append_failures_are_redacted_and_bounded() {
        let mut options = HarnessOptions::default();
        options.timestamp_source = failing_timestamp_source;
        let (context, timestamp_harness) = harness_with(
            vec![ScriptedStep::Response {
                response: text_response("cannot timestamp"),
                event: None,
            }],
            options,
        )
        .await;
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Timestamp,
                ..
            }
        ));
        assert_eq!(timestamp_harness.log.snapshot().await.entries().len(), 1);
        timestamp_harness.cleanup().await;

        let (context, closed_harness) = harness(vec![ScriptedStep::Response {
            response: text_response("closed conversation"),
            event: None,
        }])
        .await;
        closed_harness.log.close().await.unwrap();
        let result = run_turn(context).await;
        assert!(matches!(
            result,
            TurnTaskResult::Failed {
                failure: super::runner::TurnFailure::Conversation,
                ..
            }
        ));
        closed_harness.cleanup().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn response_classification_precedes_persistence_and_unknown_dispatches_by_calls() {
        for finish in [
            ModelFinishReason::Length,
            ModelFinishReason::ContentFiltered,
            ModelFinishReason::ToolCalls,
        ] {
            let (context, harness) = harness(vec![ScriptedStep::Response {
                response: ModelResponse::new(
                    vec![AssistantPart::Text("invalid".to_owned())],
                    finish,
                    None,
                )
                .unwrap(),
                event: None,
            }])
            .await;
            let result = run_turn(context).await;
            assert!(matches!(
                result,
                TurnTaskResult::Failed {
                    failure: super::runner::TurnFailure::InvalidResponse,
                    ..
                }
            ));
            assert_eq!(harness.log.snapshot().await.entries().len(), 1);
            harness.cleanup().await;
        }

        for finish in [ModelFinishReason::Stop, ModelFinishReason::Refused] {
            let (context, harness) = harness(vec![ScriptedStep::Response {
                response: ModelResponse::new(
                    vec![AssistantPart::ToolCall(tool_call(0, "alpha"))],
                    finish,
                    None,
                )
                .unwrap(),
                event: None,
            }])
            .await;
            let result = run_turn(context).await;
            assert!(matches!(
                result,
                TurnTaskResult::Failed {
                    failure: super::runner::TurnFailure::InvalidResponse,
                    ..
                }
            ));
            assert_eq!(harness.log.snapshot().await.entries().len(), 1);
            harness.cleanup().await;
        }

        for finish in [ModelFinishReason::Refused, ModelFinishReason::Unknown] {
            let (context, harness) = harness(vec![ScriptedStep::Response {
                response: ModelResponse::new(
                    vec![AssistantPart::Text("final".to_owned())],
                    finish,
                    None,
                )
                .unwrap(),
                event: None,
            }])
            .await;
            assert!(matches!(
                run_turn(context).await,
                TurnTaskResult::Completed { .. }
            ));
            assert_eq!(harness.log.snapshot().await.entries().len(), 2);
            harness.cleanup().await;
        }

        let order = Arc::new(Mutex::new(Vec::new()));
        let (registry, enabled, _) = tool_registry(vec![TestTool::new(
            "alpha",
            ToolBehavior::Success("unknown dispatch".to_owned()),
            Arc::clone(&order),
        )]);
        let mut options = HarnessOptions::default();
        options.registry = registry;
        options.enabled_tools = enabled;
        let (context, harness) = harness_with(
            vec![
                ScriptedStep::Response {
                    response: ModelResponse::new(
                        vec![AssistantPart::ToolCall(tool_call(0, "alpha"))],
                        ModelFinishReason::Unknown,
                        None,
                    )
                    .unwrap(),
                    event: None,
                },
                ScriptedStep::Response {
                    response: text_response("after unknown tool"),
                    event: None,
                },
            ],
            options,
        )
        .await;
        assert!(matches!(
            run_turn(context).await,
            TurnTaskResult::Completed { .. }
        ));
        assert_eq!(
            *order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["alpha"]
        );
        assert_eq!(harness.log.snapshot().await.entries().len(), 4);
        harness.cleanup().await;
    }
}
