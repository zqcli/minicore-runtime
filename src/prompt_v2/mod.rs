mod builder;
mod compaction;

pub(crate) use builder::{PromptBuildOptions, PromptBuilder, PromptError};
pub(crate) use compaction::{
    CompactionConfig, CompactionError, CompactionPlan, Compactor, Plan, ValidatedSummary,
    append_validated_summary,
};

const _: () = {
    let _ = std::mem::size_of::<PromptError>();
    let _ = std::mem::size_of::<PromptBuilder>();
    let _ = std::mem::size_of::<PromptBuildOptions>();
    let _ = std::mem::size_of::<CompactionError>();
    let _ = std::mem::size_of::<CompactionConfig>();
    let _ = std::mem::size_of::<Compactor>();
    let _ = std::mem::size_of::<CompactionPlan>();
    let _ = std::mem::size_of::<Plan>();
    let _ = std::mem::size_of::<ValidatedSummary>();
    let _: fn(&str, &str) -> Result<PromptBuilder, PromptError> =
        |system, coding| PromptBuilder::new(system, coding);
    let _ = PromptBuilder::system_prompt;
    let _ = PromptBuilder::coding_instructions;
    let _ = PromptBuilder::build;
    let _ = PromptBuilder::estimate_tokens;
    let _ = PromptBuildOptions::new;
    let _ = PromptBuildOptions::selection;
    let _ = PromptBuildOptions::limits;
    let _ = PromptBuildOptions::reasoning;
    let _ = CompactionConfig::new;
    let _ = CompactionConfig::trigger_tokens;
    let _ = CompactionConfig::target_tokens;
    let _ = Compactor::new;
    let _ = Compactor::config;
    let _ = Compactor::plan;
    let _ = Compactor::plan_after_context_overflow;
    let _ = CompactionPlan::request;
    let _ = CompactionPlan::clone_request;
    let _ = CompactionPlan::through_seq;
    let _ = CompactionPlan::snapshot_seq;
    let _ = CompactionPlan::current_turn_messages;
    let _ = CompactionPlan::validate_summary;
    let _ = ValidatedSummary::text;
    let _ = ValidatedSummary::into_text;
    let _ = append_validated_summary;
};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::ids_v2::{SessionId, TurnId};
    use crate::model_v2::{
        AssistantPart, ModelFinishReason, ModelLimits, ModelMessage, ModelResponse, ModelSelection,
        ReasoningContent, ReasoningPreference,
    };
    use crate::session_v2::conversation::{
        ConversationError, ConversationLog, NewConversationEntry, StoredTurnOutcome,
    };
    use crate::session_v2::store::{
        SessionStore, StoredCompactionConfig, StoredExecutionConfig, StoredModelConfig,
        StoredSessionConfig,
    };
    use crate::session_v2::time::Timestamp;
    use crate::tools_v2::{ToolName, ToolSpec};

    fn timestamp() -> Timestamp {
        "2026-08-19T12:34:56.789Z".parse().unwrap()
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("minicore-p5-prompt-{}", SessionId::new().unwrap()))
    }

    fn config(id: SessionId) -> StoredSessionConfig {
        let model = StoredModelConfig::new(ModelSelection::new(
            "anthropic".parse().unwrap(),
            "claude".parse().unwrap(),
        ));
        let execution = StoredExecutionConfig::new(
            BTreeSet::new(),
            StoredCompactionConfig::new(100, 50).unwrap(),
            4,
        )
        .unwrap();
        StoredSessionConfig::new(
            id,
            timestamp(),
            timestamp(),
            PathBuf::from("/tmp/workspace"),
            model,
            "stored system".to_owned(),
            execution,
        )
        .unwrap()
    }

    async fn opened() -> (SessionStore, ConversationLog, PathBuf, SessionId) {
        let root = root();
        let store = SessionStore::open(root.clone()).await.unwrap();
        let id = SessionId::new().unwrap();
        store.create(&config(id)).await.unwrap();
        let log = ConversationLog::open(&store, id).await.unwrap();
        (store, log, root, id)
    }

    async fn cleanup(store: &SessionStore, log: &ConversationLog, root: PathBuf) {
        log.close().await.unwrap();
        store.shutdown().await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn builder() -> PromptBuilder {
        PromptBuilder::new("system", "coding instructions").unwrap()
    }

    fn options(limits: ModelLimits, reasoning: ReasoningPreference) -> PromptBuildOptions {
        PromptBuildOptions::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
            limits,
            reasoning,
        )
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec::new(
            name.parse::<ToolName>().unwrap(),
            "tool description",
            json!({"type": "object"}),
        )
        .unwrap()
    }

    fn user(turn_id: TurnId, text: &str) -> NewConversationEntry {
        NewConversationEntry::User {
            turn_id,
            timestamp: timestamp(),
            text: text.to_owned(),
        }
    }

    fn assistant(turn_id: TurnId, text: &str) -> NewConversationEntry {
        NewConversationEntry::Assistant {
            turn_id,
            timestamp: timestamp(),
            text: Some(text.to_owned()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        }
    }

    fn terminal(turn_id: TurnId) -> NewConversationEntry {
        NewConversationEntry::TurnTerminal {
            turn_id,
            timestamp: timestamp(),
            outcome: StoredTurnOutcome::Completed,
        }
    }

    async fn completed_and_current() -> (
        SessionStore,
        ConversationLog,
        PathBuf,
        SessionId,
        crate::session_v2::conversation::CompactionConversationView,
    ) {
        let (store, log, root, id) = opened().await;
        let completed_turn = TurnId::new().unwrap();
        log.append(user(completed_turn, "old question"))
            .await
            .unwrap();
        log.append(assistant(completed_turn, "old answer"))
            .await
            .unwrap();
        log.append(terminal(completed_turn)).await.unwrap();
        let current_turn = TurnId::new().unwrap();
        log.append(user(current_turn, "current question"))
            .await
            .unwrap();
        let view = log.compaction_view().await.unwrap();
        (store, log, root, id, view)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builder_orders_messages_and_forwards_selection_limits_reasoning_and_tools() {
        let (store, log, root, _) = opened().await;
        let completed_turn = TurnId::new().unwrap();
        log.append(user(completed_turn, "old question"))
            .await
            .unwrap();
        log.append(assistant(completed_turn, "old answer"))
            .await
            .unwrap();
        log.append(terminal(completed_turn)).await.unwrap();
        log.append_summary(3, 3, timestamp(), "prior summary".to_owned())
            .await
            .unwrap();
        let current_turn = TurnId::new().unwrap();
        log.append(user(current_turn, "current question"))
            .await
            .unwrap();
        let view = log.prompt_view().await.unwrap();
        let alpha = tool("alpha");
        let beta = tool("beta");
        let limits = ModelLimits::new(Some(4096), Some(128)).unwrap();
        let request = builder()
            .build(
                &view,
                &[alpha.clone(), beta.clone()],
                options(limits, ReasoningPreference::Low),
            )
            .unwrap();
        assert_eq!(
            request.messages(),
            &[
                ModelMessage::system("system").unwrap(),
                ModelMessage::system("coding instructions").unwrap(),
                ModelMessage::user("prior summary").unwrap(),
                ModelMessage::user("current question").unwrap(),
            ]
        );
        assert_eq!(request.tools(), &[alpha, beta]);
        assert_eq!(request.selection().provider_id().as_str(), "openai");
        assert_eq!(request.selection().model_id().as_str(), "gpt-5");
        assert_eq!(request.limits(), &limits);
        assert_eq!(request.reasoning(), ReasoningPreference::Low);
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builder_checks_text_and_strict_tool_order() {
        assert!(matches!(
            PromptBuilder::new("", ""),
            Err(PromptError::InvalidText)
        ));
        assert!(matches!(
            PromptBuilder::new("bad\u{0000}", "coding"),
            Err(PromptError::InvalidText)
        ));
        assert!(matches!(
            PromptBuilder::new("x".repeat(262_145), "coding"),
            Err(PromptError::InvalidText)
        ));
        assert!(
            PromptBuilder::new("", "coding")
                .unwrap()
                .system_prompt()
                .is_none()
        );
        let builder = builder();
        let (store, log, root, _) = opened().await;
        let view = log.prompt_view().await.unwrap();
        let options = options(ModelLimits::default(), ReasoningPreference::Auto);
        assert!(matches!(
            builder.build(&view, &[tool("beta"), tool("alpha")], options.clone()),
            Err(PromptError::InvalidTools)
        ));
        assert!(matches!(
            builder.build(&view, &[tool("alpha"), tool("alpha")], options),
            Err(PromptError::InvalidTools)
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn builder_estimator_has_exact_context_boundary_and_unknown_context_passes() {
        let (store, log, root, _) = opened().await;
        let view = log.prompt_view().await.unwrap();
        let builder = builder();
        let tools = [tool("alpha")];
        let input_tokens = builder.estimate_tokens(&view, &tools).unwrap();
        let equality =
            ModelLimits::new(Some(u32::try_from(input_tokens + 7).unwrap()), Some(7)).unwrap();
        assert!(
            builder
                .build(&view, &tools, options(equality, ReasoningPreference::Auto))
                .is_ok()
        );
        let below =
            ModelLimits::new(Some(u32::try_from(input_tokens + 6).unwrap()), Some(7)).unwrap();
        assert!(matches!(
            builder.build(&view, &tools, options(below, ReasoningPreference::Auto)),
            Err(PromptError::ContextOverflow)
        ));
        let unknown = ModelLimits::new(None, Some(7)).unwrap();
        assert!(
            builder
                .build(&view, &tools, options(unknown, ReasoningPreference::Auto))
                .is_ok()
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_builds_summary_request_and_preserves_current_messages() {
        let (store, log, root, _id, view) = completed_and_current().await;
        let config = CompactionConfig::new(81, 70).unwrap();
        let compactor = Compactor::new(config);
        let active_limits = ModelLimits::new(Some(4096), Some(256)).unwrap();
        let plan = compactor
            .plan(
                &builder(),
                &view,
                &[tool("alpha")],
                options(active_limits, ReasoningPreference::High),
            )
            .unwrap()
            .unwrap();
        assert_eq!(plan.through_seq(), 3);
        assert_eq!(plan.snapshot_seq(), 4);
        assert_eq!(
            plan.current_turn_messages(),
            &[ModelMessage::user("current question").unwrap()]
        );
        let request = plan.request();
        assert_eq!(request.reasoning(), ReasoningPreference::Disabled);
        assert!(request.tools().is_empty());
        assert_eq!(request.selection().provider_id().as_str(), "openai");
        assert_eq!(request.limits(), &active_limits);
        assert_eq!(
            request.messages(),
            &[
                ModelMessage::system("system").unwrap(),
                ModelMessage::system("coding instructions").unwrap(),
                ModelMessage::user("old question").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("old answer".to_owned())])
                    .unwrap(),
                ModelMessage::user(
                    "Summarize the preceding conversation. Return only the summary text."
                )
                .unwrap(),
            ]
        );
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_returns_none_below_trigger_and_rejects_without_terminal() {
        let (store, log, root, _id, view) = completed_and_current().await;
        let none = Compactor::new(CompactionConfig::new(10_000, 100).unwrap())
            .plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            )
            .unwrap();
        assert!(none.is_none());
        cleanup(&store, &log, root).await;

        let (store, log, root, _) = opened().await;
        let turn_id = TurnId::new().unwrap();
        log.append(user(turn_id, "only current")).await.unwrap();
        let view = log.compaction_view().await.unwrap();
        assert!(matches!(
            Compactor::new(CompactionConfig::new(20, 10).unwrap()).plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            ),
            Err(CompactionError::NotReady)
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_includes_existing_summary_before_new_completed_messages() {
        let (store, log, root, _) = opened().await;
        let first_turn = TurnId::new().unwrap();
        log.append(user(first_turn, "first question"))
            .await
            .unwrap();
        log.append(assistant(first_turn, "first answer"))
            .await
            .unwrap();
        log.append(terminal(first_turn)).await.unwrap();
        log.append_summary(3, 3, timestamp(), "existing summary".to_owned())
            .await
            .unwrap();
        let second_turn = TurnId::new().unwrap();
        log.append(user(second_turn, "second question"))
            .await
            .unwrap();
        log.append(assistant(second_turn, "second answer"))
            .await
            .unwrap();
        log.append(terminal(second_turn)).await.unwrap();
        let current_turn = TurnId::new().unwrap();
        log.append(user(current_turn, "current question"))
            .await
            .unwrap();
        let view = log.compaction_view().await.unwrap();
        let plan = Compactor::new(CompactionConfig::new(60, 50).unwrap())
            .plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            )
            .unwrap()
            .unwrap();
        assert_eq!(plan.through_seq(), 7);
        assert_eq!(plan.snapshot_seq(), 8);
        assert_eq!(
            plan.request().messages(),
            &[
                ModelMessage::system("system").unwrap(),
                ModelMessage::system("coding instructions").unwrap(),
                ModelMessage::user("existing summary").unwrap(),
                ModelMessage::user("second question").unwrap(),
                ModelMessage::assistant(vec![AssistantPart::Text("second answer".to_owned())])
                    .unwrap(),
                ModelMessage::user(
                    "Summarize the preceding conversation. Return only the summary text."
                )
                .unwrap(),
            ]
        );
        assert_eq!(plan.request().limits(), &ModelLimits::default());
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_validates_finish_shape_text_and_target_feasibility() {
        let (store, log, root, _id, view) = completed_and_current().await;
        let compactor = Compactor::new(CompactionConfig::new(60, 50).unwrap());
        let plan = compactor
            .plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            )
            .unwrap()
            .unwrap();
        let length = ModelResponse::new(
            vec![AssistantPart::Text("summary".to_owned())],
            ModelFinishReason::Length,
            None,
        )
        .unwrap();
        assert!(matches!(
            plan.validate_summary(&length),
            Err(CompactionError::InvalidSummaryFinish)
        ));
        let multiple = ModelResponse::new(
            vec![
                AssistantPart::Text("one".to_owned()),
                AssistantPart::Text("two".to_owned()),
            ],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap();
        assert!(matches!(
            plan.validate_summary(&multiple),
            Err(CompactionError::InvalidSummaryShape)
        ));
        let reasoning = ModelResponse::new(
            vec![AssistantPart::Reasoning(
                ReasoningContent::new(Some("thinking".to_owned()), None, None, None, None).unwrap(),
            )],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap();
        assert!(matches!(
            plan.validate_summary(&reasoning),
            Err(CompactionError::InvalidSummaryShape)
        ));
        let too_long = ModelResponse::new(
            vec![AssistantPart::Text("x".repeat(65_537))],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap();
        assert!(matches!(
            plan.validate_summary(&too_long),
            Err(CompactionError::InvalidSummaryText)
        ));
        cleanup(&store, &log, root).await;

        let (store, log, root, _id, view) = completed_and_current().await;
        assert!(matches!(
            Compactor::new(CompactionConfig::new(60, 1).unwrap()).plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            ),
            Err(CompactionError::TargetTooSmall)
        ));
        cleanup(&store, &log, root).await;
    }

    #[test]
    fn compaction_config_requires_nonzero_target_below_nonzero_trigger() {
        assert!(matches!(
            CompactionConfig::new(0, 1),
            Err(CompactionError::InvalidConfig)
        ));
        assert!(matches!(
            CompactionConfig::new(1, 0),
            Err(CompactionError::InvalidConfig)
        ));
        assert!(matches!(
            CompactionConfig::new(1, 1),
            Err(CompactionError::InvalidConfig)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_rejects_summary_request_that_cannot_fit_known_context() {
        let (store, log, root, _id, view) = completed_and_current().await;
        assert!(matches!(
            Compactor::new(CompactionConfig::new(60, 50).unwrap()).plan(
                &builder(),
                &view,
                &[],
                options(
                    ModelLimits::new(Some(1), Some(1)).unwrap(),
                    ReasoningPreference::Auto,
                ),
            ),
            Err(CompactionError::PostSummaryContextOverflow)
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compactor_preflights_large_current_turn_before_summary_request() {
        let (store, log, root, _) = opened().await;
        let completed_turn = TurnId::new().unwrap();
        log.append(user(completed_turn, "completed question"))
            .await
            .unwrap();
        log.append(assistant(completed_turn, "completed answer"))
            .await
            .unwrap();
        log.append(terminal(completed_turn)).await.unwrap();
        let current_turn = TurnId::new().unwrap();
        log.append(user(current_turn, &"current ".repeat(6_000)))
            .await
            .unwrap();
        let view = log.compaction_view().await.unwrap();
        assert!(matches!(
            Compactor::new(CompactionConfig::new(10_000, 5_000).unwrap()).plan(
                &builder(),
                &view,
                &[],
                options(
                    ModelLimits::new(Some(100), None).unwrap(),
                    ReasoningPreference::Auto,
                ),
            ),
            Err(CompactionError::PostSummaryContextOverflow)
        ));
        cleanup(&store, &log, root).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validated_summary_append_is_stale_safe_append_only_and_preserves_current_prompt() {
        let (store, log, root, id, view) = completed_and_current().await;
        let plan = Compactor::new(CompactionConfig::new(60, 50).unwrap())
            .plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            )
            .unwrap()
            .unwrap();
        let response = ModelResponse::new(
            vec![AssistantPart::Text("compact result".to_owned())],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap();
        let validated = plan.validate_summary(&response).unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let before = fs::read(&file).unwrap();
        append_validated_summary(&log, &plan, timestamp(), &validated)
            .await
            .unwrap();
        let after = fs::read(&file).unwrap();
        assert!(after.starts_with(&before));
        let prompt = log.prompt_view().await.unwrap();
        assert_eq!(prompt.latest_summary().unwrap().text(), "compact result");
        assert_eq!(
            prompt.messages(),
            &[ModelMessage::user("current question").unwrap()]
        );
        cleanup(&store, &log, root).await;

        let (store, log, root, id, view) = completed_and_current().await;
        let plan = Compactor::new(CompactionConfig::new(60, 50).unwrap())
            .plan(
                &builder(),
                &view,
                &[],
                options(ModelLimits::default(), ReasoningPreference::Auto),
            )
            .unwrap()
            .unwrap();
        log.append(user(TurnId::new().unwrap(), "newer current"))
            .await
            .unwrap();
        let file = root
            .join("sessions")
            .join(id.to_string())
            .join("conversation.jsonl");
        let before = fs::read(&file).unwrap();
        let response = ModelResponse::new(
            vec![AssistantPart::Text("stale".to_owned())],
            ModelFinishReason::Stop,
            None,
        )
        .unwrap();
        let validated = plan.validate_summary(&response).unwrap();
        assert_eq!(
            append_validated_summary(&log, &plan, timestamp(), &validated).await,
            Err(ConversationError::Stale)
        );
        assert_eq!(fs::read(&file).unwrap(), before);
        cleanup(&store, &log, root).await;
    }
}
