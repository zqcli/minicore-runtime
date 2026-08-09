use std::fmt;
use std::sync::Arc;

use thiserror::Error;

use crate::agent_session_lifecycle::{AgentDefinition, AgentRevisionRef, SessionDefinition};
use crate::compaction::{
    Compaction, CompactionError, CompactionModelBasis, CompactionPlan, CompactionPlanInput,
    CompactionPressure, CompactionPressureInput, CompactionSettingsSnapshot, CompactionTrigger,
    LiveCompactionSourceView,
};
use crate::live_conversation::LiveConversationView;
use crate::model_gateway::{
    ModelCatalogView, ModelGateway, ModelResolutionErrorKind, ResolveTurnModelRequest,
    TurnModelSnapshot,
};
use crate::prompt::{
    AgentRunCompactionAssemblyBasis, AssembledModelContext, CanonicalUserMessage,
    CompactionSummaryAssemblyBasis, PromptAssemblyInput, PromptError, PromptIntent,
    PromptResourceView, PromptService, PromptSet, PromptTurnContext,
};
use crate::skills::SkillView;
use crate::tools::ToolSet;
use crate::wire::{SessionDefinitionRevision, SessionId, TurnId};
use crate::workspace::WorkspaceSnapshot;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TurnContextCaptureError {
    #[error("turn context identity binding is invalid")]
    InvalidBinding,
    #[error("turn model resolution failed")]
    Model(ModelResolutionErrorKind),
    #[error("turn prompt capture failed")]
    Prompt,
}

pub(crate) struct TurnContextCapture {
    pub(crate) turn_id: TurnId,
    pub(crate) session: Arc<SessionDefinition>,
    pub(crate) agent: Arc<AgentDefinition>,
    pub(crate) workspace: Arc<WorkspaceSnapshot>,
    pub(crate) prompt_service: Arc<PromptService>,
    pub(crate) prompt_resources: Arc<PromptResourceView>,
    pub(crate) model_gateway: Arc<ModelGateway>,
    pub(crate) model_catalog: Arc<ModelCatalogView>,
    pub(crate) tool_set: Arc<ToolSet>,
    pub(crate) compaction: CompactionSettingsSnapshot,
}

pub(crate) struct TurnExecutionContext {
    session_id: SessionId,
    turn_id: TurnId,
    session_revision: SessionDefinitionRevision,
    agent: AgentRevisionRef,
    agent_definition: Arc<AgentDefinition>,
    model: Arc<TurnModelSnapshot>,
    workspace: Arc<WorkspaceSnapshot>,
    skill_view: Arc<SkillView>,
    tool_set: Arc<ToolSet>,
    prompt_set: Arc<PromptSet>,
    compaction: CompactionSettingsSnapshot,
    agent_run_compaction: AgentRunCompactionAssemblyBasis,
    compaction_summary: CompactionSummaryAssemblyBasis,
    compaction_model: CompactionModelBasis,
}

impl TurnExecutionContext {
    pub(crate) fn capture(input: TurnContextCapture) -> Result<Arc<Self>, TurnContextCaptureError> {
        let expected_agent = input.session.agent();
        if input.session.session_id() != input.workspace.session_id()
            || input.session.workspace().revision() != input.workspace.revision()
            || input.agent.agent_id() != expected_agent.agent_id()
            || input.agent.revision() != expected_agent.revision()
        {
            return Err(TurnContextCaptureError::InvalidBinding);
        }

        let model = input
            .model_gateway
            .resolve_for_turn(
                input.model_catalog,
                ResolveTurnModelRequest::new(
                    input.session.model().selection().clone(),
                    input.session.model().reasoning(),
                    input.session.model().max_output_tokens(),
                ),
            )
            .map_err(|error| TurnContextCaptureError::Model(error.kind()))?;
        let skill_view = SkillView::empty();
        let tool_set = input.tool_set;
        let prompt_set = input
            .prompt_service
            .for_turn(PromptTurnContext::new(
                expected_agent,
                input.session.session_id(),
                input.session.revision(),
                input.prompt_resources,
                input.agent.prompts().clone(),
                input.session.prompts().clone(),
                input.workspace.prompt_context(),
                tool_set.prompt_view(),
                skill_view.prompt_view(),
                Arc::clone(&model),
            ))
            .map_err(|_| TurnContextCaptureError::Prompt)?;
        let agent_run_compaction = prompt_set
            .agent_run_compaction_assembly_basis()
            .map_err(|_| TurnContextCaptureError::Prompt)?;
        let compaction_summary = prompt_set
            .compaction_summary_assembly_basis()
            .map_err(|_| TurnContextCaptureError::Prompt)?;
        let compaction_model = CompactionModelBasis::from_turn_model(&model);

        Ok(Arc::new(Self {
            session_id: input.session.session_id(),
            turn_id: input.turn_id,
            session_revision: input.session.revision(),
            agent: expected_agent,
            agent_definition: input.agent,
            model,
            workspace: input.workspace,
            skill_view,
            tool_set,
            prompt_set,
            compaction: input.compaction,
            agent_run_compaction,
            compaction_summary,
            compaction_model,
        }))
    }

    pub(crate) async fn resolve_user_message(
        &self,
        intent: PromptIntent,
    ) -> Result<CanonicalUserMessage, PromptError> {
        self.prompt_set.compose_user_message(intent)
    }

    pub(crate) fn assemble_agent_run(
        &self,
        conversation: &LiveConversationView,
    ) -> Result<Arc<AssembledModelContext>, PromptError> {
        self.prompt_set
            .assemble(PromptAssemblyInput::agent_run(conversation, None))
            .map(Arc::new)
    }

    #[allow(
        dead_code,
        reason = "consumed by the adjacent M10 ActiveTurnTask compaction slice"
    )]
    pub(crate) fn compaction_pressure(
        &self,
        source: &LiveCompactionSourceView,
        trigger: CompactionTrigger,
        compactions_started: u8,
    ) -> CompactionPressure {
        Compaction.pressure(CompactionPressureInput {
            source,
            settings: &self.compaction,
            agent_run: &self.agent_run_compaction,
            model: &self.compaction_model,
            trigger,
            compactions_started,
        })
    }

    #[allow(
        dead_code,
        reason = "consumed by the adjacent M10 ActiveTurnTask compaction slice"
    )]
    pub(crate) fn plan_compaction(
        &self,
        source: Arc<LiveCompactionSourceView>,
        trigger: CompactionTrigger,
        compactions_started: u8,
    ) -> Result<Arc<CompactionPlan>, CompactionError> {
        Compaction.plan(CompactionPlanInput {
            source,
            settings: self.compaction.clone(),
            agent_run: self.agent_run_compaction,
            summary_assembly: self.compaction_summary.clone(),
            model: self.compaction_model.clone(),
            trigger,
            compactions_started,
        })
    }

    #[allow(
        dead_code,
        reason = "consumed by the adjacent M10 ActiveTurnTask compaction slice"
    )]
    pub(crate) fn assemble_compaction(
        &self,
        plan: &Arc<CompactionPlan>,
    ) -> Result<Arc<AssembledModelContext>, PromptError> {
        if !plan
            .model()
            .turn_model()
            .is_exact(&self.model.turn_model_ref())
        {
            return Err(PromptError::invalid_contribution());
        }
        self.prompt_set
            .assemble(PromptAssemblyInput::compaction_summary(
                plan.summary_source(),
                plan.directive(),
                plan.budget(),
            ))
            .map(Arc::new)
    }

    pub(crate) const fn tool_set(&self) -> &Arc<ToolSet> {
        &self.tool_set
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn agent(&self) -> AgentRevisionRef {
        self.agent
    }

    pub(crate) const fn model(&self) -> &Arc<TurnModelSnapshot> {
        &self.model
    }
}

impl fmt::Debug for TurnExecutionContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnExecutionContext")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("session_revision", &self.session_revision)
            .field("agent", &self.agent)
            .field(
                "agent_prompt_count",
                &self.agent_definition.prompts().enabled().len(),
            )
            .field("model", &self.model)
            .field("workspace", &self.workspace)
            .field("skill_view", &self.skill_view)
            .field("tool_set", &self.tool_set)
            .field("prompt_set", &self.prompt_set)
            .field("compaction", &self.compaction)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use crate::agent_session_lifecycle::{
        AgentDefinition, AgentRevisionRef, SessionDefinition, SessionModelConfig,
    };
    use crate::compaction::{
        Compaction, CompactionSettings, CompactionTrigger, CompactionUnitKind,
        LiveCompactionSourceView, PreparedLiveCompactionUnit,
    };
    use crate::live_conversation::ConversationRevision;
    use crate::model_gateway::{
        ModelCallPurpose, ModelCallRequest, ModelProgressPublisher,
        ModelRequestValidationErrorKind, ModelResolutionErrorKind, ModelSelection,
        ReasoningPreference, ScriptedModelFixture,
    };
    use crate::prompt::{
        AgentPromptSelection, ModelMessage, ModelMessageRef, PromptBodyIntent, PromptErrorKind,
        PromptIntent, PromptService, SessionPromptSelection, TextIntent,
    };
    use crate::tools::ToolSet;
    use crate::wire::{
        AgentId, AgentRevision, EntryId, SessionDefinitionRevision, SessionId, Timestamp, TurnId,
        WorkspaceRevision,
    };
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy,
        lower_workspace, prompt_candidate_for_test,
    };

    use super::{TurnContextCapture, TurnContextCaptureError, TurnExecutionContext};

    fn session_id(value: u8) -> SessionId {
        format!("ses_{value:032x}").parse().unwrap()
    }

    fn turn_id(value: u8) -> TurnId {
        format!("trn_{value:032x}").parse().unwrap()
    }

    fn agent_id(value: u8) -> AgentId {
        format!("agt_{value:032x}").parse().unwrap()
    }

    fn timestamp() -> Timestamp {
        "2026-08-08T00:00:00.000Z".parse().unwrap()
    }

    fn workspace(revision: u64) -> Workspace {
        let root_key: WorkspaceRootKey = "repo".parse().unwrap();
        lower_workspace(
            WorkspaceDefinitionInput::new(
                WorkspaceRootInput::new(
                    root_key.clone(),
                    "file:///minicore-context-test".parse().unwrap(),
                    RequestedFilesystemAccess::ReadOnly,
                    WorkspaceSourcePolicy::new(false, false),
                ),
                Vec::new(),
                WorkspaceCwdSpec::new(root_key, "".parse().unwrap()),
            )
            .unwrap(),
            WorkspaceRevision::new(NonZeroU64::new(revision).unwrap()),
            WorkspacePathTarget::current(),
        )
        .unwrap()
    }

    async fn capture_parts(
        selected_session_id: SessionId,
        selected_agent_id: AgentId,
        agent_revision: u64,
        workspace_revision: u64,
    ) -> (
        Arc<SessionDefinition>,
        Arc<AgentDefinition>,
        Arc<crate::workspace::WorkspaceSnapshot>,
        Arc<PromptService>,
        Arc<crate::prompt::PromptResourceView>,
        ScriptedModelFixture,
    ) {
        capture_parts_with_responses(
            selected_session_id,
            selected_agent_id,
            agent_revision,
            workspace_revision,
            Vec::new(),
        )
        .await
    }

    async fn capture_parts_with_responses(
        selected_session_id: SessionId,
        selected_agent_id: AgentId,
        agent_revision: u64,
        workspace_revision: u64,
        responses: Vec<&str>,
    ) -> (
        Arc<SessionDefinition>,
        Arc<AgentDefinition>,
        Arc<crate::workspace::WorkspaceSnapshot>,
        Arc<PromptService>,
        Arc<crate::prompt::PromptResourceView>,
        ScriptedModelFixture,
    ) {
        let revision = AgentRevision::new(NonZeroU64::new(agent_revision).unwrap());
        let agent = Arc::new(AgentDefinition::new(
            selected_agent_id,
            revision,
            AgentPromptSelection::new(Vec::new()).unwrap(),
            timestamp(),
        ));
        let session = Arc::new(SessionDefinition::new(
            selected_session_id,
            SessionDefinitionRevision::new(NonZeroU64::new(1).unwrap()),
            AgentRevisionRef::new(selected_agent_id, revision),
            workspace(workspace_revision),
            SessionModelConfig::new(
                ModelSelection::new("openai".parse().unwrap(), "gpt-5".parse().unwrap()),
                ReasoningPreference::Auto,
                Some(NonZeroU32::new(256).unwrap()),
            ),
            SessionPromptSelection::new(Vec::new()).unwrap(),
            timestamp(),
        ));
        let workspace =
            prompt_candidate_for_test(selected_session_id, vec!["repo".parse().unwrap()])
                .with_revision_for_test(WorkspaceRevision::new(
                    NonZeroU64::new(workspace_revision).unwrap(),
                ))
                .finish(Arc::from([]), Arc::from([]))
                .unwrap();
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::new(responses);
        (
            session,
            agent,
            workspace,
            prompt_service,
            prompt_resources,
            model,
        )
    }

    async fn compaction_fixture(
        responses: Vec<&str>,
    ) -> (
        Arc<TurnExecutionContext>,
        Arc<LiveCompactionSourceView>,
        ScriptedModelFixture,
    ) {
        let selected_session_id = session_id(1);
        let (session, agent, workspace, prompt_service, prompt_resources, model) =
            capture_parts_with_responses(selected_session_id, agent_id(1), 7, 9, responses).await;
        let settings = CompactionSettings {
            pressure_reserve_tokens: NonZeroU32::new(8).unwrap(),
            summary_min_output_tokens: NonZeroU32::new(5).unwrap(),
            summary_max_output_tokens: NonZeroU32::new(10).unwrap(),
            minimum_reclaimed_tokens: NonZeroU32::new(5).unwrap(),
            summary_safety_reserve_tokens: NonZeroU32::new(2).unwrap(),
            ..CompactionSettings::default()
        };
        let context = TurnExecutionContext::capture(TurnContextCapture {
            turn_id: turn_id(1),
            session,
            agent,
            workspace,
            prompt_service,
            prompt_resources,
            model_gateway: Arc::clone(model.gateway()),
            model_catalog: Arc::clone(model.catalog()),
            tool_set: ToolSet::empty(),
            compaction: settings.validate().unwrap(),
        })
        .unwrap();
        let unit = PreparedLiveCompactionUnit::for_live_reducer(
            CompactionUnitKind::UserMessage,
            Arc::from([
                ModelMessage::unstamped_user_text(Arc::from("history ".repeat(256))).unwrap(),
            ]),
        )
        .unwrap()
        .bind_origin(
            "ent_11111111111111111111111111111111"
                .parse::<EntryId>()
                .unwrap(),
        );
        let source = Arc::new(
            LiveCompactionSourceView::for_live_reducer(
                selected_session_id,
                ConversationRevision::default(),
                Arc::from([unit]),
            )
            .unwrap(),
        );
        (context, source, model)
    }

    #[tokio::test]
    async fn capture_binds_exact_session_agent_workspace_prompt_and_model_objects() {
        let selected_session_id = session_id(1);
        let selected_agent_id = agent_id(1);
        let (session, agent, workspace, prompt_service, prompt_resources, model) =
            capture_parts(selected_session_id, selected_agent_id, 7, 9).await;

        let compaction_settings = CompactionSettings {
            pressure_reserve_tokens: NonZeroU32::new(1_234).unwrap(),
            ..CompactionSettings::default()
        };
        let context = TurnExecutionContext::capture(TurnContextCapture {
            turn_id: turn_id(1),
            session,
            agent,
            workspace,
            prompt_service,
            prompt_resources,
            model_gateway: Arc::clone(model.gateway()),
            model_catalog: Arc::clone(model.catalog()),
            tool_set: ToolSet::empty(),
            compaction: compaction_settings.validate().unwrap(),
        })
        .unwrap();

        assert_eq!(context.session_id(), selected_session_id);
        assert_eq!(context.turn_id(), turn_id(1));
        assert_eq!(context.agent().agent_id(), selected_agent_id);
        assert_eq!(context.agent().revision().get(), 7);
        assert_eq!(context.compaction.pressure_reserve_tokens().get(), 1_234);
        let future_settings = CompactionSettings {
            pressure_reserve_tokens: NonZeroU32::new(9_999).unwrap(),
            ..compaction_settings
        };
        assert_eq!(future_settings.pressure_reserve_tokens.get(), 9_999);
        assert_eq!(context.compaction.pressure_reserve_tokens().get(), 1_234);
        let message = context
            .resolve_user_message(
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(message.message().content()[0].as_text(), "hello");
    }

    #[tokio::test]
    async fn compaction_assembly_binds_the_exact_plan_budget_and_model_request() {
        let (context, source, _) = compaction_fixture(Vec::new()).await;
        let plan = context
            .plan_compaction(
                Arc::clone(&source),
                CompactionTrigger::ProviderContextOverflow,
                0,
            )
            .unwrap();

        let assembled = context.assemble_compaction(&plan).unwrap();
        let proof = assembled.assembly_proof();
        assert_eq!(proof.purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(proof.source_revision(), *source.revision());
        assert_eq!(assembled.system().len(), 1);
        assert_eq!(assembled.system()[0].text(), "required");
        assert!(assembled.tools_empty());
        assert_eq!(assembled.messages().len(), 2);
        assert_eq!(
            proof.compaction_summary_budget().unwrap().budget(),
            plan.budget()
        );

        let request = ModelCallRequest::new(
            Arc::clone(context.model()),
            ModelCallPurpose::CompactionSummary,
            assembled,
            *source.revision(),
            Some(plan.budget().max_output_tokens()),
        )
        .unwrap();
        assert_eq!(request.purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(
            request.effective_max_output_tokens(),
            plan.budget().max_output_tokens()
        );
        for (purpose, max_output_tokens) in [
            (
                ModelCallPurpose::AgentRun,
                Some(plan.budget().max_output_tokens()),
            ),
            (ModelCallPurpose::CompactionSummary, None),
            (
                ModelCallPurpose::CompactionSummary,
                NonZeroU32::new(plan.budget().max_output_tokens().get() - 1),
            ),
        ] {
            let error = ModelCallRequest::new(
                Arc::clone(context.model()),
                purpose,
                Arc::clone(request.input()),
                *source.revision(),
                max_output_tokens,
            )
            .unwrap_err();
            assert_eq!(
                error.kind(),
                ModelRequestValidationErrorKind::AssemblyMismatch
            );
        }

        let (other_context, _, _) = compaction_fixture(Vec::new()).await;
        assert_eq!(
            other_context.assemble_compaction(&plan).unwrap_err().kind(),
            PromptErrorKind::InvalidContribution
        );
    }

    #[tokio::test]
    async fn validated_compaction_summary_seals_exact_automatic_provenance_and_replacement() {
        let (context, source, model) = compaction_fixture(vec!["portable summary"]).await;
        let plan = context
            .plan_compaction(
                Arc::clone(&source),
                CompactionTrigger::ProviderContextOverflow,
                0,
            )
            .unwrap();
        let assembled = context.assemble_compaction(&plan).unwrap();
        let request = Arc::new(
            ModelCallRequest::new(
                Arc::clone(context.model()),
                ModelCallPurpose::CompactionSummary,
                assembled,
                *source.revision(),
                Some(plan.budget().max_output_tokens()),
            )
            .unwrap(),
        );
        let result = model
            .gateway()
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let validated = Compaction
            .validate_summary(Arc::clone(&plan), &result, 1)
            .unwrap();
        assert!(Arc::ptr_eq(validated.plan(), &plan));
        let replacement = validated.into_replacement().unwrap();
        let (stored, rolling_summary) = replacement.into_parts();
        assert_eq!(stored.summary(), "portable summary");
        assert_eq!(stored.first_kept_entry_id(), None);
        let call = stored.model_call().unwrap();
        assert_eq!(call.model(), result.response().model());
        assert_eq!(
            call.requested_max_output_tokens(),
            plan.budget().max_output_tokens()
        );
        assert_eq!(call.logical_retry_count(), 1);
        match rolling_summary.as_ref() {
            ModelMessageRef::User { content } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].as_text(), "portable summary");
            }
            _ => panic!("validated summary must become one user-role rolling summary"),
        }
    }

    #[tokio::test]
    async fn capture_rejects_every_cross_binding_mismatch() {
        let selected_session_id = session_id(1);
        let selected_agent_id = agent_id(1);
        let (session, agent, workspace, prompt_service, prompt_resources, model) =
            capture_parts(selected_session_id, selected_agent_id, 7, 9).await;

        let wrong_session_workspace =
            prompt_candidate_for_test(session_id(2), vec!["repo".parse().unwrap()])
                .with_revision_for_test(WorkspaceRevision::new(NonZeroU64::new(9).unwrap()))
                .finish(Arc::from([]), Arc::from([]))
                .unwrap();
        let result = TurnExecutionContext::capture(TurnContextCapture {
            turn_id: turn_id(1),
            session: Arc::clone(&session),
            agent: Arc::clone(&agent),
            workspace: wrong_session_workspace,
            prompt_service: Arc::clone(&prompt_service),
            prompt_resources: Arc::clone(&prompt_resources),
            model_gateway: Arc::clone(model.gateway()),
            model_catalog: Arc::clone(model.catalog()),
            tool_set: ToolSet::empty(),
            compaction: CompactionSettings::default().validate().unwrap(),
        });
        assert_eq!(result.unwrap_err(), TurnContextCaptureError::InvalidBinding);

        let wrong_agent = Arc::new(AgentDefinition::new(
            selected_agent_id,
            AgentRevision::new(NonZeroU64::new(8).unwrap()),
            AgentPromptSelection::new(Vec::new()).unwrap(),
            timestamp(),
        ));
        let result = TurnExecutionContext::capture(TurnContextCapture {
            turn_id: turn_id(1),
            session: Arc::clone(&session),
            agent: wrong_agent,
            workspace: Arc::clone(&workspace),
            prompt_service: Arc::clone(&prompt_service),
            prompt_resources: Arc::clone(&prompt_resources),
            model_gateway: Arc::clone(model.gateway()),
            model_catalog: Arc::clone(model.catalog()),
            tool_set: ToolSet::empty(),
            compaction: CompactionSettings::default().validate().unwrap(),
        });
        assert_eq!(result.unwrap_err(), TurnContextCaptureError::InvalidBinding);

        let other_model = ScriptedModelFixture::new(Vec::new());
        let result = TurnExecutionContext::capture(TurnContextCapture {
            turn_id: turn_id(1),
            session,
            agent,
            workspace,
            prompt_service,
            prompt_resources,
            model_gateway: Arc::clone(model.gateway()),
            model_catalog: Arc::clone(other_model.catalog()),
            tool_set: ToolSet::empty(),
            compaction: CompactionSettings::default().validate().unwrap(),
        });
        assert_eq!(
            result.unwrap_err(),
            TurnContextCaptureError::Model(ModelResolutionErrorKind::CatalogUnavailable)
        );
    }
}
