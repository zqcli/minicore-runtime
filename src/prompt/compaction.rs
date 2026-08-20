use std::fmt;

use thiserror::Error;

use crate::model::{
    AssistantPart, ModelFinishReason, ModelMessage, ModelRequest, ModelResponse,
    ReasoningPreference,
};
use crate::storage::conversation::{
    CompactionConversationView, ConversationError, ConversationLog,
};
use crate::storage::time::Timestamp;
use crate::tools::ToolSpec;

use super::builder::{MAX_SUMMARY_TEXT_BYTES, PromptBuildOptions, PromptBuilder, PromptError};

const SUMMARY_INSTRUCTION: &str =
    "Summarize the preceding conversation. Return only the summary text.";
const MINIMUM_SAFE_SUMMARY: &str = "x";

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CompactionError {
    #[error("compaction configuration is invalid")]
    InvalidConfig,
    #[error("compaction requires a completed terminal boundary")]
    NotReady,
    #[error("compaction target cannot fit the preserved conversation")]
    TargetTooSmall,
    #[error("compaction summary request exceeds the known context window")]
    SummaryContextOverflow,
    #[error("validated summary cannot fit the known context window")]
    PostSummaryContextOverflow,
    #[error("summary response did not stop normally")]
    InvalidSummaryFinish,
    #[error("summary response must contain exactly one text part")]
    InvalidSummaryShape,
    #[error("summary text is invalid")]
    InvalidSummaryText,
    #[error("compaction prompt assembly failed")]
    Prompt(PromptError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompactionConfig {
    trigger_tokens: u64,
    target_tokens: u64,
}

impl CompactionConfig {
    pub(crate) fn new(trigger_tokens: u64, target_tokens: u64) -> Result<Self, CompactionError> {
        if trigger_tokens == 0 || target_tokens == 0 || target_tokens >= trigger_tokens {
            return Err(CompactionError::InvalidConfig);
        }
        Ok(Self {
            trigger_tokens,
            target_tokens,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Compactor {
    config: CompactionConfig,
}

impl Compactor {
    pub(crate) const fn new(config: CompactionConfig) -> Self {
        Self { config }
    }

    pub(crate) fn plan(
        &self,
        builder: &PromptBuilder,
        view: &CompactionConversationView,
        tools: &[ToolSpec],
        options: PromptBuildOptions,
    ) -> Result<Option<CompactionPlan>, CompactionError> {
        self.plan_internal(builder, view, tools, options, false)
    }

    pub(crate) fn plan_after_context_overflow(
        &self,
        builder: &PromptBuilder,
        view: &CompactionConversationView,
        tools: &[ToolSpec],
        options: PromptBuildOptions,
    ) -> Result<CompactionPlan, CompactionError> {
        self.plan_internal(builder, view, tools, options, true)?
            .ok_or(CompactionError::NotReady)
    }

    fn plan_internal(
        &self,
        builder: &PromptBuilder,
        view: &CompactionConversationView,
        tools: &[ToolSpec],
        options: PromptBuildOptions,
        forced: bool,
    ) -> Result<Option<CompactionPlan>, CompactionError> {
        let mut ordinary_messages = Vec::with_capacity(
            view.completed_messages()
                .len()
                .saturating_add(view.current_turn_messages().len()),
        );
        ordinary_messages.extend(view.completed_messages().iter().cloned());
        ordinary_messages.extend(view.current_turn_messages().iter().cloned());
        let ordinary_estimate = builder
            .estimate_parts(
                view.latest_summary().map(|summary| summary.text()),
                &ordinary_messages,
                tools,
            )
            .map_err(CompactionError::Prompt)?;
        if !forced && ordinary_estimate <= self.config.trigger_tokens {
            return Ok(None);
        }
        let Some(through_seq) = view.through_seq() else {
            return Err(CompactionError::NotReady);
        };
        if view.completed_messages().is_empty() {
            return Err(CompactionError::NotReady);
        }
        let _minimum_post_summary = builder
            .build_parts(
                Some(MINIMUM_SAFE_SUMMARY),
                view.current_turn_messages(),
                tools,
                &options,
            )
            .map_err(map_post_summary_prompt_error)?;
        let minimum_post_summary = builder
            .estimate_parts(
                Some(MINIMUM_SAFE_SUMMARY),
                view.current_turn_messages(),
                tools,
            )
            .map_err(CompactionError::Prompt)?;
        if minimum_post_summary > self.config.target_tokens {
            return Err(CompactionError::TargetTooSmall);
        }

        let mut summary_messages = view.completed_messages().to_vec();
        summary_messages.push(
            ModelMessage::user(SUMMARY_INSTRUCTION)
                .map_err(|_| CompactionError::Prompt(PromptError::InvalidRequest))?,
        );
        let summary_options = PromptBuildOptions::new(
            options.selection().clone(),
            options.limits(),
            ReasoningPreference::Disabled,
        );
        let request = builder
            .build_parts(
                view.latest_summary().map(|summary| summary.text()),
                &summary_messages,
                &[],
                &summary_options,
            )
            .map_err(map_summary_prompt_error)?;
        Ok(Some(CompactionPlan {
            request,
            through_seq,
            snapshot_seq: view.snapshot_seq(),
            builder: builder.clone(),
            options,
            tools: tools.to_vec(),
            current_turn_messages: view.current_turn_messages().to_vec(),
            target_tokens: self.config.target_tokens,
        }))
    }
}

#[derive(Clone)]
pub(crate) struct CompactionPlan {
    request: ModelRequest,
    through_seq: u64,
    snapshot_seq: u64,
    builder: PromptBuilder,
    options: PromptBuildOptions,
    tools: Vec<ToolSpec>,
    current_turn_messages: Vec<ModelMessage>,
    target_tokens: u64,
}

impl CompactionPlan {
    pub(crate) fn clone_request(&self) -> ModelRequest {
        self.request.clone()
    }

    pub(crate) fn validate_summary(
        &self,
        response: &ModelResponse,
    ) -> Result<ValidatedSummary, CompactionError> {
        if response.finish_reason() != ModelFinishReason::Stop {
            return Err(CompactionError::InvalidSummaryFinish);
        }
        let [AssistantPart::Text(text)] = response.parts() else {
            return Err(CompactionError::InvalidSummaryShape);
        };
        if !valid_summary_text(text) {
            return Err(CompactionError::InvalidSummaryText);
        }
        let post_summary = self
            .builder
            .build_parts(
                Some(text),
                &self.current_turn_messages,
                &self.tools,
                &self.options,
            )
            .map_err(map_post_summary_prompt_error)?;
        let estimated_post_summary = self
            .builder
            .estimate_parts(Some(text), &self.current_turn_messages, &self.tools)
            .map_err(CompactionError::Prompt)?;
        if estimated_post_summary > self.target_tokens {
            return Err(CompactionError::TargetTooSmall);
        }
        let _ = post_summary;
        Ok(ValidatedSummary {
            text: text.to_owned(),
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ValidatedSummary {
    text: String,
}

impl fmt::Debug for ValidatedSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedSummary")
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

pub(crate) async fn append_validated_summary(
    log: &ConversationLog,
    plan: &CompactionPlan,
    timestamp: Timestamp,
    summary: &ValidatedSummary,
) -> Result<u64, ConversationError> {
    log.append_summary(
        plan.snapshot_seq,
        plan.through_seq,
        timestamp,
        summary.text.clone(),
    )
    .await
}

fn valid_summary_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SUMMARY_TEXT_BYTES
        && value
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

fn map_summary_prompt_error(error: PromptError) -> CompactionError {
    match error {
        PromptError::ContextOverflow => CompactionError::SummaryContextOverflow,
        other => CompactionError::Prompt(other),
    }
}

fn map_post_summary_prompt_error(error: PromptError) -> CompactionError {
    match error {
        PromptError::ContextOverflow => CompactionError::PostSummaryContextOverflow,
        other => CompactionError::Prompt(other),
    }
}
