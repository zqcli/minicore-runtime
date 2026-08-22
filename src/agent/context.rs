use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::config::RetryPolicy;
use crate::ids::{SessionId, TurnId};
use crate::model::{ModelGateway, ModelLimits, ReasoningPreference};
use crate::prompt::{Compactor, PromptBuildOptions, PromptBuilder};
use crate::storage::conversation::ConversationLog;
use crate::time::{Timestamp, TimestampError};
use crate::tools::{InteractionClient, ToolName, ToolPolicy, ToolRegistry, ToolSpec};
use crate::workspace::Workspace;

use super::runner::RunnerEventSink;

pub(crate) type TimestampSource = fn() -> Result<Timestamp, TimestampError>;

pub(crate) const fn system_timestamp_source() -> TimestampSource {
    Timestamp::now_utc
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum TurnContextError {
    #[error("turn model selection is unavailable")]
    ModelUnavailable,
    #[error("turn model limits or reasoning preference are invalid")]
    InvalidModelConfiguration,
    #[error("turn enabled tool is unavailable")]
    UnknownTool,
    #[error("turn tool-round limit is invalid")]
    InvalidToolRounds,
}

pub(crate) struct TurnContextDependencies {
    pub(crate) prompt_builder: PromptBuilder,
    pub(crate) prompt_options: PromptBuildOptions,
    pub(crate) compactor: Compactor,
    pub(crate) gateway: ModelGateway,
    pub(crate) tools: ToolRegistry,
    pub(crate) policy: Arc<dyn ToolPolicy>,
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) conversation: Arc<ConversationLog>,
    pub(crate) interactions: InteractionClient,
    pub(crate) cancellation: CancellationToken,
    pub(crate) timestamp_source: TimestampSource,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) events: RunnerEventSink,
}

pub(crate) struct TurnContext {
    session_id: SessionId,
    turn_id: TurnId,
    prompt_builder: PromptBuilder,
    prompt_options: PromptBuildOptions,
    compactor: Compactor,
    gateway: ModelGateway,
    tools: ToolRegistry,
    policy: Arc<dyn ToolPolicy>,
    enabled_tools: BTreeSet<ToolName>,
    tool_specs: Vec<ToolSpec>,
    max_tool_rounds: u8,
    workspace: Arc<Workspace>,
    conversation: Arc<ConversationLog>,
    interactions: InteractionClient,
    cancellation: CancellationToken,
    timestamp_source: TimestampSource,
    retry_policy: RetryPolicy,
    events: RunnerEventSink,
}

impl fmt::Debug for TurnContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnContext")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("enabled_tool_count", &self.enabled_tools.len())
            .field("max_tool_rounds", &self.max_tool_rounds)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl TurnContext {
    pub(crate) fn new(
        session_id: SessionId,
        turn_id: TurnId,
        enabled_tools: BTreeSet<ToolName>,
        max_tool_rounds: u8,
        dependencies: TurnContextDependencies,
    ) -> Result<Self, TurnContextError> {
        if !(1..=64).contains(&max_tool_rounds) {
            return Err(TurnContextError::InvalidToolRounds);
        }
        let resolved = dependencies
            .gateway
            .resolve(dependencies.prompt_options.selection())
            .map_err(|_| TurnContextError::ModelUnavailable)?;
        if !limits_fit(
            dependencies.prompt_options.limits(),
            *resolved.descriptor().limits(),
        ) || !resolved
            .descriptor()
            .supports_reasoning(dependencies.prompt_options.reasoning())
            || !resolved
                .descriptor()
                .supports_reasoning(ReasoningPreference::Disabled)
        {
            return Err(TurnContextError::InvalidModelConfiguration);
        }
        let tool_specs = dependencies
            .tools
            .specs(&enabled_tools)
            .map_err(|_| TurnContextError::UnknownTool)?;
        Ok(Self {
            session_id,
            turn_id,
            prompt_builder: dependencies.prompt_builder,
            prompt_options: dependencies.prompt_options,
            compactor: dependencies.compactor,
            gateway: dependencies.gateway,
            tools: dependencies.tools,
            policy: dependencies.policy,
            enabled_tools,
            tool_specs,
            max_tool_rounds,
            workspace: dependencies.workspace,
            conversation: dependencies.conversation,
            interactions: dependencies.interactions,
            cancellation: dependencies.cancellation,
            timestamp_source: dependencies.timestamp_source,
            retry_policy: dependencies.retry_policy,
            events: dependencies.events,
        })
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub(crate) const fn prompt_builder(&self) -> &PromptBuilder {
        &self.prompt_builder
    }

    pub(crate) const fn prompt_options(&self) -> &PromptBuildOptions {
        &self.prompt_options
    }

    pub(crate) const fn compactor(&self) -> &Compactor {
        &self.compactor
    }

    pub(crate) const fn gateway(&self) -> &ModelGateway {
        &self.gateway
    }

    pub(crate) const fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    pub(crate) const fn policy(&self) -> &Arc<dyn ToolPolicy> {
        &self.policy
    }

    pub(crate) const fn enabled_tools(&self) -> &BTreeSet<ToolName> {
        &self.enabled_tools
    }

    pub(crate) fn tool_specs(&self) -> &[ToolSpec] {
        &self.tool_specs
    }

    pub(crate) const fn max_tool_rounds(&self) -> u8 {
        self.max_tool_rounds
    }

    pub(crate) const fn workspace(&self) -> &Arc<Workspace> {
        &self.workspace
    }

    pub(crate) const fn conversation(&self) -> &Arc<ConversationLog> {
        &self.conversation
    }

    pub(crate) const fn interactions(&self) -> &InteractionClient {
        &self.interactions
    }

    pub(crate) const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) fn timestamp(&self) -> Result<Timestamp, TimestampError> {
        (self.timestamp_source)()
    }

    pub(crate) const fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    pub(crate) const fn events(&self) -> &RunnerEventSink {
        &self.events
    }
}

fn limits_fit(requested: ModelLimits, available: ModelLimits) -> bool {
    requested
        .context_window_tokens()
        .zip(available.context_window_tokens())
        .is_none_or(|(requested, available)| requested <= available)
        && requested
            .max_output_tokens()
            .zip(available.max_output_tokens())
            .is_none_or(|(requested, available)| requested <= available)
}

const _: () = {
    let _ = std::mem::size_of::<RetryPolicy>();
    let _ = std::mem::size_of::<TurnContext>();
    let _ = std::mem::size_of::<TurnContextDependencies>();
    let _ = system_timestamp_source;
    let _ = RetryPolicy::new;
    let _ = RetryPolicy::max_attempts;
    let _ = RetryPolicy::base_delay;
    let _ = RetryPolicy::delay_for_retry;
    let _ = TurnContext::new;
    let _ = TurnContext::session_id;
    let _ = TurnContext::turn_id;
    let _ = TurnContext::prompt_builder;
    let _ = TurnContext::prompt_options;
    let _ = TurnContext::compactor;
    let _ = TurnContext::gateway;
    let _ = TurnContext::tools;
    let _ = TurnContext::policy;
    let _ = TurnContext::enabled_tools;
    let _ = TurnContext::tool_specs;
    let _ = TurnContext::max_tool_rounds;
    let _ = TurnContext::workspace;
    let _ = TurnContext::conversation;
    let _ = TurnContext::interactions;
    let _ = TurnContext::cancellation;
    let _ = TurnContext::timestamp;
    let _ = TurnContext::retry_policy;
    let _ = TurnContext::events;
};
