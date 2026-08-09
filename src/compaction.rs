use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::{NonZeroU8, NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use thiserror::Error;

use crate::live_conversation::ConversationRevision;
use crate::model_gateway::{
    EffectiveModelLimits, FinalizedAssistantContent, ModelCallResult, ModelFinishReason,
    ModelResponseSummary, ModelUsage, ProviderResponseId, ProviderResponseMetadata, TokenEstimator,
    TurnModelRef, TurnModelSnapshot,
};
use crate::prompt::{
    AgentRunCompactionAssemblyBasis, CompactionSummaryAssemblyBasis, ModelMessage, ModelMessageRef,
};
use crate::tools::ToolResultContent;
use crate::wire::lexical::validate_safe_text;
use crate::wire::{EntryId, SessionId};

pub(crate) const MAX_STORED_COMPACTION_SUMMARY_BYTES: usize = 65_536;
const COMPACTION_TOOL_RESULT_REDUCTION_THRESHOLD_BYTES: usize = 16 * 1_024;
const COMPACTION_TOOL_RESULT_REDUCTION_HEAD_BYTES: usize = 4 * 1_024;
const COMPACTION_TOOL_RESULT_REDUCTION_TAIL_BYTES: usize = 4 * 1_024;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompactionSummaryFormatVersion {
    V1 = 1,
}

const COMPACTION_SUMMARY_FORMAT_VERSION: CompactionSummaryFormatVersion =
    CompactionSummaryFormatVersion::V1;

impl CompactionSummaryFormatVersion {
    const fn number(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub pressure_reserve_tokens: NonZeroU32,
    pub summary_min_output_tokens: NonZeroU32,
    pub summary_max_output_tokens: NonZeroU32,
    pub minimum_reclaimed_tokens: NonZeroU32,
    pub max_compactions_per_turn: NonZeroU8,
    pub summary_safety_reserve_tokens: NonZeroU32,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            pressure_reserve_tokens: NonZeroU32::new(4_096).expect("non-zero default"),
            summary_min_output_tokens: NonZeroU32::new(512).expect("non-zero default"),
            summary_max_output_tokens: NonZeroU32::new(2_048).expect("non-zero default"),
            minimum_reclaimed_tokens: NonZeroU32::new(2_048).expect("non-zero default"),
            max_compactions_per_turn: NonZeroU8::new(4).expect("non-zero default"),
            summary_safety_reserve_tokens: NonZeroU32::new(512).expect("non-zero default"),
        }
    }
}

impl CompactionSettings {
    pub(crate) fn validate(self) -> Result<CompactionSettingsSnapshot, CompactionSettingsError> {
        if self.summary_min_output_tokens > self.summary_max_output_tokens {
            return Err(CompactionSettingsError::InvalidSummaryOutputRange);
        }
        Ok(CompactionSettingsSnapshot(Arc::new(
            ValidatedCompactionSettings {
                enabled: self.enabled,
                pressure_reserve_tokens: self.pressure_reserve_tokens,
                summary_min_output_tokens: self.summary_min_output_tokens,
                summary_max_output_tokens: self.summary_max_output_tokens,
                minimum_reclaimed_tokens: self.minimum_reclaimed_tokens,
                max_compactions_per_turn: self.max_compactions_per_turn,
                summary_safety_reserve_tokens: self.summary_safety_reserve_tokens,
            },
        )))
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CompactionSettingsError {
    #[error("compaction summary minimum output exceeds its maximum")]
    InvalidSummaryOutputRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompactionSettingsSnapshot(Arc<ValidatedCompactionSettings>);

#[derive(Debug, Eq, PartialEq)]
struct ValidatedCompactionSettings {
    enabled: bool,
    pressure_reserve_tokens: NonZeroU32,
    summary_min_output_tokens: NonZeroU32,
    summary_max_output_tokens: NonZeroU32,
    minimum_reclaimed_tokens: NonZeroU32,
    max_compactions_per_turn: NonZeroU8,
    summary_safety_reserve_tokens: NonZeroU32,
}

impl CompactionSettingsSnapshot {
    pub(crate) fn enabled(&self) -> bool {
        self.0.enabled
    }

    pub(crate) fn pressure_reserve_tokens(&self) -> NonZeroU32 {
        self.0.pressure_reserve_tokens
    }

    pub(crate) fn summary_min_output_tokens(&self) -> NonZeroU32 {
        self.0.summary_min_output_tokens
    }

    pub(crate) fn summary_max_output_tokens(&self) -> NonZeroU32 {
        self.0.summary_max_output_tokens
    }

    pub(crate) fn minimum_reclaimed_tokens(&self) -> NonZeroU32 {
        self.0.minimum_reclaimed_tokens
    }

    pub(crate) fn max_compactions_per_turn(&self) -> NonZeroU8 {
        self.0.max_compactions_per_turn
    }

    pub(crate) fn summary_safety_reserve_tokens(&self) -> NonZeroU32 {
        self.0.summary_safety_reserve_tokens
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum CompactionValueError {
    #[error("stored compaction summary is empty, unsafe, or exceeds its byte limit")]
    Summary,
    #[error("stored compaction finish reason is not portable")]
    FinishReason,
    #[error("stored compaction logical retry count exceeds its limit")]
    LogicalRetryCount,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum CompactionUnitKind {
    RollingSummary,
    UserMessage,
    AssistantMessage,
    ToolExchange,
}

#[derive(Clone)]
pub(crate) struct LiveCompactionSourceView {
    session_id: SessionId,
    revision: ConversationRevision,
    units: Arc<[LiveCompactionUnit]>,
}

#[derive(Clone)]
pub(crate) struct LiveCompactionUnit {
    first_entry_id: EntryId,
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

pub(crate) struct PreparedLiveCompactionUnit {
    kind: CompactionUnitKind,
    messages: Arc<[ModelMessage]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionSourceErrorReason {
    EmptyUnitMessages,
    DuplicateUnitOrigin,
    MisplacedRollingSummary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CompactionSourceError {
    reason: CompactionSourceErrorReason,
}

impl CompactionSourceError {
    const fn new(reason: CompactionSourceErrorReason) -> Self {
        Self { reason }
    }
}

impl fmt::Debug for CompactionSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionSourceError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for CompactionSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid live compaction source")
    }
}

impl Error for CompactionSourceError {}

impl PreparedLiveCompactionUnit {
    fn new(
        kind: CompactionUnitKind,
        messages: Arc<[ModelMessage]>,
    ) -> Result<Self, CompactionSourceError> {
        if messages.is_empty() {
            return Err(CompactionSourceError::new(
                CompactionSourceErrorReason::EmptyUnitMessages,
            ));
        }
        Ok(Self { kind, messages })
    }

    pub(crate) fn for_live_reducer(
        kind: CompactionUnitKind,
        messages: Arc<[ModelMessage]>,
    ) -> Result<Self, CompactionSourceError> {
        Self::new(kind, messages)
    }

    /// Constructs a stable unit from the already-sanitized cold replay projection. Replay has a
    /// separate named ingress so it cannot accidentally be mistaken for a live reducer apply.
    pub(crate) fn for_replay(
        kind: CompactionUnitKind,
        messages: Arc<[ModelMessage]>,
    ) -> Result<Self, CompactionSourceError> {
        Self::new(kind, messages)
    }

    pub(crate) fn bind_origin(self, first_entry_id: EntryId) -> LiveCompactionUnit {
        LiveCompactionUnit {
            first_entry_id,
            kind: self.kind,
            messages: self.messages,
        }
    }
}

impl LiveCompactionUnit {
    pub(crate) const fn first_entry_id(&self) -> &EntryId {
        &self.first_entry_id
    }

    pub(crate) const fn kind(&self) -> CompactionUnitKind {
        self.kind
    }

    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }
}

impl LiveCompactionSourceView {
    pub(crate) fn for_live_reducer(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> Result<Self, CompactionSourceError> {
        Self::validate_and_build(session_id, revision, units)
    }

    /// Validates the stable-unit source emitted by tolerant cold replay. This is a distinct
    /// ingress from the live reducer source even though both preserve the same source invariants.
    pub(crate) fn for_replay(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> Result<Self, CompactionSourceError> {
        Self::validate_and_build(session_id, revision, units)
    }

    fn validate_and_build(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> Result<Self, CompactionSourceError> {
        let mut origins = BTreeSet::new();
        for (index, unit) in units.iter().enumerate() {
            if unit.messages().is_empty() {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::EmptyUnitMessages,
                ));
            }
            if !origins.insert(*unit.first_entry_id()) {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::DuplicateUnitOrigin,
                ));
            }
            if unit.kind() == CompactionUnitKind::RollingSummary && index != 0 {
                return Err(CompactionSourceError::new(
                    CompactionSourceErrorReason::MisplacedRollingSummary,
                ));
            }
        }
        Ok(Self {
            session_id,
            revision,
            units,
        })
    }

    pub(crate) const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub(crate) const fn revision(&self) -> &ConversationRevision {
        &self.revision
    }

    pub(crate) fn units(&self) -> &[LiveCompactionUnit] {
        &self.units
    }

    pub(crate) fn has_same_stable_identity(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.revision == other.revision
            && self.units.len() == other.units.len()
            && self
                .units
                .iter()
                .zip(other.units.iter())
                .all(|(left, right)| {
                    left.first_entry_id == right.first_entry_id && left.kind == right.kind
                })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionTrigger {
    ProactivePressure,
    PromptContextOverflow,
    ProviderContextOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionImpossibleReason {
    Disabled,
    EmptySource,
    UnknownContextLimit,
    UnestimableSource,
    CompactionLimitReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionPressure {
    NotNeeded,
    Recommended,
    Required,
    Impossible(CompactionImpossibleReason),
}

#[derive(Clone)]
pub(crate) struct CompactionModelBasis {
    turn_model: TurnModelRef,
    model_summary: ModelResponseSummary,
    limits: EffectiveModelLimits,
    estimator: TokenEstimator,
    agent_run_output_reserve_tokens: NonZeroU32,
}

impl CompactionModelBasis {
    pub(crate) fn from_turn_model(model: &TurnModelSnapshot) -> Self {
        Self {
            turn_model: model.turn_model_ref(),
            model_summary: ModelResponseSummary::reconstruct(
                model.definition().provider_id().clone(),
                model.definition().model_id().clone(),
                model.generation().reasoning(),
                model.generation().service_class(),
            ),
            limits: model.limits(),
            estimator: model.token_estimator(),
            agent_run_output_reserve_tokens: model.generation().max_output_tokens(),
        }
    }

    pub(crate) const fn turn_model(&self) -> &TurnModelRef {
        &self.turn_model
    }

    pub(crate) const fn model_summary(&self) -> &ModelResponseSummary {
        &self.model_summary
    }

    pub(crate) const fn limits(&self) -> EffectiveModelLimits {
        self.limits
    }

    pub(crate) const fn estimator(&self) -> TokenEstimator {
        self.estimator
    }

    pub(crate) const fn agent_run_output_reserve_tokens(&self) -> NonZeroU32 {
        self.agent_run_output_reserve_tokens
    }
}

pub(crate) struct CompactionPressureInput<'a> {
    pub(crate) source: &'a LiveCompactionSourceView,
    pub(crate) settings: &'a CompactionSettingsSnapshot,
    pub(crate) agent_run: &'a AgentRunCompactionAssemblyBasis,
    pub(crate) model: &'a CompactionModelBasis,
    pub(crate) trigger: CompactionTrigger,
    pub(crate) compactions_started: u8,
}

pub(crate) struct CompactionPlanInput {
    pub(crate) source: Arc<LiveCompactionSourceView>,
    pub(crate) settings: CompactionSettingsSnapshot,
    pub(crate) agent_run: AgentRunCompactionAssemblyBasis,
    pub(crate) summary_assembly: CompactionSummaryAssemblyBasis,
    pub(crate) model: CompactionModelBasis,
    pub(crate) trigger: CompactionTrigger,
    pub(crate) compactions_started: u8,
}

#[derive(Clone)]
pub(crate) struct CompactionSummarySourceView {
    source_revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}

impl CompactionSummarySourceView {
    pub(crate) const fn source_revision(&self) -> ConversationRevision {
        self.source_revision
    }

    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }
}

#[derive(Clone)]
pub(crate) struct CompactionSummaryDirective {
    message: ModelMessage,
}

impl CompactionSummaryDirective {
    pub(crate) const fn message(&self) -> &ModelMessage {
        &self.message
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompactionSummaryBudget {
    fixed_prompt_tokens: u64,
    reduced_source_tokens: u64,
    directive_tokens: u64,
    safety_reserve_tokens: NonZeroU32,
    max_output_tokens: NonZeroU32,
}

impl CompactionSummaryBudget {
    pub(crate) const fn fixed_prompt_tokens(&self) -> u64 {
        self.fixed_prompt_tokens
    }

    pub(crate) const fn reduced_source_tokens(&self) -> u64 {
        self.reduced_source_tokens
    }

    pub(crate) const fn directive_tokens(&self) -> u64 {
        self.directive_tokens
    }

    pub(crate) const fn safety_reserve_tokens(&self) -> NonZeroU32 {
        self.safety_reserve_tokens
    }

    pub(crate) const fn max_output_tokens(&self) -> NonZeroU32 {
        self.max_output_tokens
    }
}

pub(crate) struct CompactionPlan {
    source: Arc<LiveCompactionSourceView>,
    settings: CompactionSettingsSnapshot,
    trigger: CompactionTrigger,
    summarized_unit_count: NonZeroUsize,
    summary_source: Arc<CompactionSummarySourceView>,
    directive: CompactionSummaryDirective,
    budget: CompactionSummaryBudget,
    model: CompactionModelBasis,
    estimated_before_tokens: u64,
    estimated_after_upper_bound_tokens: u64,
    estimated_reclaimed_tokens: u64,
}

impl CompactionPlan {
    pub(crate) const fn source(&self) -> &Arc<LiveCompactionSourceView> {
        &self.source
    }

    pub(crate) const fn settings(&self) -> &CompactionSettingsSnapshot {
        &self.settings
    }

    pub(crate) const fn trigger(&self) -> CompactionTrigger {
        self.trigger
    }

    pub(crate) const fn summarized_unit_count(&self) -> NonZeroUsize {
        self.summarized_unit_count
    }

    pub(crate) const fn summary_source(&self) -> &Arc<CompactionSummarySourceView> {
        &self.summary_source
    }

    pub(crate) const fn directive(&self) -> &CompactionSummaryDirective {
        &self.directive
    }

    pub(crate) const fn budget(&self) -> &CompactionSummaryBudget {
        &self.budget
    }

    pub(crate) const fn model(&self) -> &CompactionModelBasis {
        &self.model
    }

    pub(crate) fn retained_units(&self) -> &[LiveCompactionUnit] {
        &self.source.units()[self.summarized_unit_count.get()..]
    }

    pub(crate) fn first_kept_entry_id(&self) -> Option<EntryId> {
        self.source
            .units()
            .get(self.summarized_unit_count.get())
            .map(|unit| *unit.first_entry_id())
    }

    pub(crate) const fn estimated_before_tokens(&self) -> u64 {
        self.estimated_before_tokens
    }

    pub(crate) const fn estimated_after_upper_bound_tokens(&self) -> u64 {
        self.estimated_after_upper_bound_tokens
    }

    pub(crate) const fn estimated_reclaimed_tokens(&self) -> u64 {
        self.estimated_reclaimed_tokens
    }
}

impl fmt::Debug for CompactionPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionPlan")
            .field("trigger", &self.trigger)
            .field("summarized_unit_count", &self.summarized_unit_count)
            .field("budget", &self.budget)
            .field("estimated_before_tokens", &self.estimated_before_tokens)
            .field(
                "estimated_after_upper_bound_tokens",
                &self.estimated_after_upper_bound_tokens,
            )
            .field(
                "estimated_reclaimed_tokens",
                &self.estimated_reclaimed_tokens,
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionErrorReason {
    NotNeeded,
    Impossible(CompactionImpossibleReason),
    MismatchedEstimator,
    ArithmeticOverflow,
    InvalidDirective,
    NoFeasibleSummaryBudget,
    NoFeasiblePostReplace,
    InsufficientReclaim,
    InvalidSummarySourceReduction,
    InvalidSummary,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CompactionError {
    reason: CompactionErrorReason,
}

impl CompactionError {
    const fn new(reason: CompactionErrorReason) -> Self {
        Self { reason }
    }

    pub(crate) const fn reason(self) -> CompactionErrorReason {
        self.reason
    }
}

impl fmt::Debug for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("compaction planning failed")
    }
}

impl Error for CompactionError {}

pub(crate) struct Compaction;

impl Compaction {
    pub(crate) fn pressure(&self, input: CompactionPressureInput<'_>) -> CompactionPressure {
        let hard_trigger = !matches!(input.trigger, CompactionTrigger::ProactivePressure);
        if !input.settings.enabled() {
            return if hard_trigger {
                CompactionPressure::Impossible(CompactionImpossibleReason::Disabled)
            } else {
                CompactionPressure::NotNeeded
            };
        }
        if input.source.units().is_empty() {
            return if hard_trigger {
                CompactionPressure::Impossible(CompactionImpossibleReason::EmptySource)
            } else {
                CompactionPressure::NotNeeded
            };
        }
        let Some(context_window) = input.model.limits().context_window_tokens() else {
            return if hard_trigger {
                CompactionPressure::Impossible(CompactionImpossibleReason::UnknownContextLimit)
            } else {
                CompactionPressure::NotNeeded
            };
        };

        let estimated_units = match estimate_stable_units(input.source, input.model.estimator()) {
            Some(estimates) => estimates,
            None => {
                return if hard_trigger {
                    CompactionPressure::Impossible(CompactionImpossibleReason::UnestimableSource)
                } else {
                    CompactionPressure::NotNeeded
                };
            }
        };
        if hard_trigger
            && input.compactions_started >= input.settings.max_compactions_per_turn().get()
        {
            return CompactionPressure::Impossible(
                CompactionImpossibleReason::CompactionLimitReached,
            );
        }
        if hard_trigger {
            return CompactionPressure::Required;
        }

        let estimated_input = estimated_units
            .iter()
            .try_fold(input.agent_run.fixed_input_tokens(), |total, estimate| {
                total.checked_add(*estimate)
            });
        let Some(estimated_input) = estimated_input else {
            return required_pressure(input.settings, input.compactions_started);
        };
        let context_window = u64::from(context_window.get());
        if estimated_input >= context_window {
            return required_pressure(input.settings, input.compactions_started);
        }
        let effective_headroom = u64::from(
            input
                .settings
                .pressure_reserve_tokens()
                .max(input.model.agent_run_output_reserve_tokens())
                .get(),
        );
        match estimated_input.checked_add(effective_headroom) {
            None => required_pressure(input.settings, input.compactions_started),
            Some(with_headroom) if with_headroom >= context_window => {
                CompactionPressure::Recommended
            }
            Some(_) => CompactionPressure::NotNeeded,
        }
    }

    pub(crate) fn plan(
        &self,
        input: CompactionPlanInput,
    ) -> Result<Arc<CompactionPlan>, CompactionError> {
        if input.agent_run.estimator() != input.model.estimator()
            || input.summary_assembly.estimator() != input.model.estimator()
        {
            return Err(CompactionError::new(
                CompactionErrorReason::MismatchedEstimator,
            ));
        }

        match self.pressure(CompactionPressureInput {
            source: &input.source,
            settings: &input.settings,
            agent_run: &input.agent_run,
            model: &input.model,
            trigger: input.trigger,
            compactions_started: input.compactions_started,
        }) {
            CompactionPressure::NotNeeded => {
                return Err(CompactionError::new(CompactionErrorReason::NotNeeded));
            }
            CompactionPressure::Impossible(reason) => {
                return Err(CompactionError::new(CompactionErrorReason::Impossible(
                    reason,
                )));
            }
            CompactionPressure::Recommended
                if input.compactions_started >= input.settings.max_compactions_per_turn().get() =>
            {
                return Err(CompactionError::new(CompactionErrorReason::Impossible(
                    CompactionImpossibleReason::CompactionLimitReached,
                )));
            }
            CompactionPressure::Recommended | CompactionPressure::Required => {}
        }

        let Some(context_window) = input.model.limits().context_window_tokens() else {
            return Err(CompactionError::new(CompactionErrorReason::Impossible(
                CompactionImpossibleReason::UnknownContextLimit,
            )));
        };
        let unit_estimates = estimate_stable_units(&input.source, input.model.estimator())
            .ok_or_else(|| {
                CompactionError::new(CompactionErrorReason::Impossible(
                    CompactionImpossibleReason::UnestimableSource,
                ))
            })?;
        let stable_tokens = unit_estimates
            .iter()
            .try_fold(0_u64, |total, estimate| total.checked_add(*estimate));
        let Some(stable_tokens) = stable_tokens else {
            return Err(CompactionError::new(
                CompactionErrorReason::ArithmeticOverflow,
            ));
        };
        let Some(estimated_before_tokens) = input
            .agent_run
            .fixed_input_tokens()
            .checked_add(stable_tokens)
        else {
            return Err(CompactionError::new(
                CompactionErrorReason::ArithmeticOverflow,
            ));
        };

        let directive = CompactionSummaryDirective {
            message: ModelMessage::unstamped_user_text(Arc::from(
                "Summarize the supplied stable conversation prefix into portable text. Preserve user intent, decisions, constraints, tool call identifiers, important results, and unresolved work. Return only the summary.",
            ))
            .map_err(|_| CompactionError::new(CompactionErrorReason::InvalidDirective))?,
        };
        let directive_tokens = directive
            .message()
            .compaction_estimated_tokens(input.model.estimator())
            .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
        let context_window = u64::from(context_window.get());
        let effective_headroom = u64::from(
            input
                .settings
                .pressure_reserve_tokens()
                .max(input.model.agent_run_output_reserve_tokens())
                .get(),
        );
        let mut reduced_units = Vec::new();
        let mut summarized_original_tokens = 0_u64;
        let mut summarized_reduced_tokens = 0_u64;
        let mut saw_summary_budget = false;
        let mut saw_post_replace_fit = false;

        for (index, unit_estimate) in unit_estimates.iter().enumerate() {
            let reduced_messages = reduce_summary_unit(&input.source.units()[index])?;
            let reduced_unit_estimate =
                estimate_summary_unit(&reduced_messages, input.model.estimator()).ok_or_else(
                    || CompactionError::new(CompactionErrorReason::ArithmeticOverflow),
                )?;
            reduced_units.push(reduced_messages);
            summarized_original_tokens = summarized_original_tokens
                .checked_add(*unit_estimate)
                .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            summarized_reduced_tokens = summarized_reduced_tokens
                .checked_add(reduced_unit_estimate)
                .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            let available_output = context_window
                .checked_sub(input.summary_assembly.fixed_prompt_tokens())
                .and_then(|available| available.checked_sub(summarized_reduced_tokens))
                .and_then(|available| available.checked_sub(directive_tokens))
                .and_then(|available| {
                    available.checked_sub(u64::from(
                        input.settings.summary_safety_reserve_tokens().get(),
                    ))
                });
            let Some(available_output) = available_output else {
                continue;
            };
            let available_output = u32::try_from(available_output)
                .map_err(|_| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            let mut max_output_tokens = input.settings.summary_max_output_tokens().get();
            if let Some(model_maximum) = input.model.limits().max_output_tokens() {
                max_output_tokens = max_output_tokens.min(model_maximum.get());
            }
            max_output_tokens = max_output_tokens.min(available_output);
            let Some(max_output_tokens) = NonZeroU32::new(max_output_tokens) else {
                continue;
            };
            if max_output_tokens < input.settings.summary_min_output_tokens() {
                continue;
            }
            saw_summary_budget = true;

            let retained_tokens = stable_tokens
                .checked_sub(summarized_original_tokens)
                .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            let estimated_after_upper_bound_tokens = input
                .agent_run
                .fixed_input_tokens()
                .checked_add(input.agent_run.rolling_summary_message_overhead_tokens())
                .and_then(|total| total.checked_add(u64::from(max_output_tokens.get())))
                .and_then(|total| total.checked_add(retained_tokens))
                .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            let post_replace_with_headroom = estimated_after_upper_bound_tokens
                .checked_add(effective_headroom)
                .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
            if post_replace_with_headroom > context_window {
                continue;
            }
            saw_post_replace_fit = true;
            let Some(estimated_reclaimed_tokens) =
                estimated_before_tokens.checked_sub(estimated_after_upper_bound_tokens)
            else {
                continue;
            };
            if estimated_reclaimed_tokens
                < u64::from(input.settings.minimum_reclaimed_tokens().get())
            {
                continue;
            }

            let summarized_unit_count = NonZeroUsize::new(index + 1)
                .expect("candidate iteration always produces a non-zero cut");
            let messages: Arc<[ModelMessage]> = reduced_units[..summarized_unit_count.get()]
                .iter()
                .flat_map(|messages| messages.iter().cloned())
                .collect::<Vec<_>>()
                .into();
            return Ok(Arc::new(CompactionPlan {
                source: Arc::clone(&input.source),
                settings: input.settings.clone(),
                trigger: input.trigger,
                summarized_unit_count,
                summary_source: Arc::new(CompactionSummarySourceView {
                    source_revision: *input.source.revision(),
                    messages,
                }),
                directive,
                budget: CompactionSummaryBudget {
                    fixed_prompt_tokens: input.summary_assembly.fixed_prompt_tokens(),
                    reduced_source_tokens: summarized_reduced_tokens,
                    directive_tokens,
                    safety_reserve_tokens: input.settings.summary_safety_reserve_tokens(),
                    max_output_tokens,
                },
                model: input.model.clone(),
                estimated_before_tokens,
                estimated_after_upper_bound_tokens,
                estimated_reclaimed_tokens,
            }));
        }

        Err(CompactionError::new(if !saw_summary_budget {
            CompactionErrorReason::NoFeasibleSummaryBudget
        } else if !saw_post_replace_fit {
            CompactionErrorReason::NoFeasiblePostReplace
        } else {
            CompactionErrorReason::InsufficientReclaim
        }))
    }

    pub(crate) fn validate_summary(
        &self,
        plan: Arc<CompactionPlan>,
        result: &ModelCallResult,
        logical_retry_count: u8,
    ) -> Result<ValidatedCompactionSummary, CompactionError> {
        let response = result.response();
        if response.model() != plan.model().model_summary()
            || !matches!(
                response.finish_reason(),
                ModelFinishReason::Stop | ModelFinishReason::Unknown
            )
            || response.effective_max_output_tokens() != plan.budget().max_output_tokens()
            || logical_retry_count > 1
        {
            return Err(CompactionError::new(CompactionErrorReason::InvalidSummary));
        }

        let mut summary = None;
        for content in response.content() {
            match content {
                FinalizedAssistantContent::Reasoning(_) => {}
                FinalizedAssistantContent::Text { text } if summary.is_none() => {
                    summary = Some(Arc::clone(text));
                }
                FinalizedAssistantContent::Text { .. }
                | FinalizedAssistantContent::ToolCall { .. } => {
                    return Err(CompactionError::new(CompactionErrorReason::InvalidSummary));
                }
            }
        }
        let summary =
            summary.ok_or_else(|| CompactionError::new(CompactionErrorReason::InvalidSummary))?;
        let model_call = StoredCompactionModelCall::new(
            response.model().clone(),
            response.response_id().cloned(),
            response.usage().cloned(),
            response.finish_reason(),
            plan.budget().max_output_tokens(),
            logical_retry_count,
            response.metadata().clone(),
        )
        .map_err(|_| CompactionError::new(CompactionErrorReason::InvalidSummary))?;
        let stored = StoredCompaction::new(summary, plan.first_kept_entry_id(), Some(model_call))
            .map_err(|_| CompactionError::new(CompactionErrorReason::InvalidSummary))?;

        Ok(ValidatedCompactionSummary { plan, stored })
    }
}

fn required_pressure(
    settings: &CompactionSettingsSnapshot,
    compactions_started: u8,
) -> CompactionPressure {
    if compactions_started >= settings.max_compactions_per_turn().get() {
        CompactionPressure::Impossible(CompactionImpossibleReason::CompactionLimitReached)
    } else {
        CompactionPressure::Required
    }
}

fn estimate_stable_units(
    source: &LiveCompactionSourceView,
    estimator: TokenEstimator,
) -> Option<Vec<u64>> {
    source
        .units()
        .iter()
        .map(|unit| {
            unit.messages().iter().try_fold(0_u64, |total, message| {
                total.checked_add(message.compaction_estimated_tokens(estimator)?)
            })
        })
        .collect()
}

fn reduce_summary_unit(unit: &LiveCompactionUnit) -> Result<Arc<[ModelMessage]>, CompactionError> {
    unit.messages()
        .iter()
        .map(reduce_summary_message)
        .collect::<Result<Vec<_>, _>>()
        .map(Arc::from)
}

fn estimate_summary_unit(messages: &[ModelMessage], estimator: TokenEstimator) -> Option<u64> {
    messages.iter().try_fold(0_u64, |total, message| {
        total.checked_add(message.compaction_estimated_tokens(estimator)?)
    })
}

fn reduce_summary_message(message: &ModelMessage) -> Result<ModelMessage, CompactionError> {
    let ModelMessageRef::Tool {
        tool_call_id,
        content,
    } = message.as_ref()
    else {
        return Ok(message.clone());
    };
    let original_bytes = content.parts().iter().try_fold(0_usize, |total, part| {
        total.checked_add(part.as_text().len())
    });
    let Some(original_bytes) = original_bytes else {
        return Err(CompactionError::new(
            CompactionErrorReason::ArithmeticOverflow,
        ));
    };
    if original_bytes <= COMPACTION_TOOL_RESULT_REDUCTION_THRESHOLD_BYTES {
        return Ok(message.clone());
    }

    let (head, head_bytes) = tool_result_head(content, COMPACTION_TOOL_RESULT_REDUCTION_HEAD_BYTES);
    let (tail, tail_bytes) = tool_result_tail(content, COMPACTION_TOOL_RESULT_REDUCTION_TAIL_BYTES);
    let kept_bytes = head_bytes
        .checked_add(tail_bytes)
        .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
    let omitted_bytes = original_bytes
        .checked_sub(kept_bytes)
        .ok_or_else(|| CompactionError::new(CompactionErrorReason::ArithmeticOverflow))?;
    let metadata = format!(
        "[minicore_compaction_tool_result]\nformat_version={}\noriginal_bytes={original_bytes}\nomitted_bytes={omitted_bytes}",
        COMPACTION_SUMMARY_FORMAT_VERSION.number(),
    );
    let reduced = ToolResultContent::from_text_parts(vec![
        metadata,
        format!("[head]\n{head}"),
        format!("[tail]\n{tail}"),
    ])
    .map_err(|_| CompactionError::new(CompactionErrorReason::InvalidSummarySourceReduction))?;
    Ok(ModelMessage::tool_result(tool_call_id.clone(), reduced))
}

fn tool_result_head(content: &ToolResultContent, maximum_bytes: usize) -> (String, usize) {
    let mut output = String::new();
    let mut kept = 0_usize;
    for (index, part) in content.parts().iter().enumerate() {
        let remaining = maximum_bytes.saturating_sub(kept);
        if remaining == 0 {
            break;
        }
        let text = part.as_text();
        let prefix = utf8_prefix(text, remaining);
        if !prefix.is_empty() {
            append_tool_result_part(&mut output, index, prefix);
            kept += prefix.len();
        }
        if prefix.len() < text.len() {
            break;
        }
    }
    (output, kept)
}

fn tool_result_tail(content: &ToolResultContent, maximum_bytes: usize) -> (String, usize) {
    let mut kept = 0_usize;
    let mut segments = Vec::new();
    for (index, part) in content.parts().iter().enumerate().rev() {
        let remaining = maximum_bytes.saturating_sub(kept);
        if remaining == 0 {
            break;
        }
        let text = part.as_text();
        let suffix = utf8_suffix(text, remaining);
        if !suffix.is_empty() {
            segments.push((index, suffix));
            kept += suffix.len();
        }
        if suffix.len() < text.len() {
            break;
        }
    }
    segments.reverse();
    let mut output = String::new();
    for (index, suffix) in segments {
        append_tool_result_part(&mut output, index, suffix);
    }
    (output, kept)
}

fn append_tool_result_part(output: &mut String, index: usize, text: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str("[part=");
    output.push_str(&index.to_string());
    output.push_str("]\n");
    output.push_str(text);
}

fn utf8_prefix(text: &str, maximum_bytes: usize) -> &str {
    let mut end = text.len().min(maximum_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn utf8_suffix(text: &str, maximum_bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(maximum_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

pub(crate) struct ValidatedCompactionSummary {
    plan: Arc<CompactionPlan>,
    stored: StoredCompaction,
}

impl ValidatedCompactionSummary {
    pub(crate) const fn plan(&self) -> &Arc<CompactionPlan> {
        &self.plan
    }

    pub(crate) fn into_replacement(self) -> Result<CompactionReplacement, CompactionError> {
        let rolling_summary = ModelMessage::rolling_summary(Arc::clone(&self.stored.summary))
            .map_err(|_| CompactionError::new(CompactionErrorReason::InvalidSummary))?;
        Ok(CompactionReplacement {
            stored: self.stored,
            rolling_summary,
        })
    }
}

impl fmt::Debug for ValidatedCompactionSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedCompactionSummary")
            .field("plan", &self.plan)
            .field("summary", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredCompaction {
    summary: Arc<str>,
    first_kept_entry_id: Option<EntryId>,
    model_call: Option<StoredCompactionModelCall>,
}

impl StoredCompaction {
    fn new(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: Option<StoredCompactionModelCall>,
    ) -> Result<Self, CompactionValueError> {
        let summary = summary.as_ref();
        validate_safe_text(summary, MAX_STORED_COMPACTION_SUMMARY_BYTES, false)
            .map_err(|_| CompactionValueError::Summary)?;
        Ok(Self {
            summary: summary.into(),
            first_kept_entry_id,
            model_call,
        })
    }

    pub(crate) fn reconstruct(
        summary: impl AsRef<str>,
        first_kept_entry_id: Option<EntryId>,
        model_call: Option<StoredCompactionModelCall>,
    ) -> Result<Self, CompactionValueError> {
        Self::new(summary, first_kept_entry_id, model_call)
    }

    /// Constructs an otherwise ordinary stored fact with a deliberately unchecked summary so
    /// the M4 replacement seam can prove its own pre-reducer validation boundary.
    #[cfg(test)]
    pub(crate) fn with_unchecked_summary_for_m4_test(
        summary: Arc<str>,
        first_kept_entry_id: Option<EntryId>,
    ) -> Self {
        Self {
            summary,
            first_kept_entry_id,
            model_call: None,
        }
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[allow(dead_code, reason = "consumed by Conversation codec/replay in M3/M5")]
    pub const fn first_kept_entry_id(&self) -> Option<EntryId> {
        self.first_kept_entry_id
    }

    pub const fn model_call(&self) -> Option<&StoredCompactionModelCall> {
        self.model_call.as_ref()
    }
}

impl fmt::Debug for StoredCompaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCompaction")
            .field("summary_bytes", &self.summary.len())
            .field(
                "has_first_kept_entry_id",
                &self.first_kept_entry_id.is_some(),
            )
            .field("has_model_call", &self.model_call.is_some())
            .finish()
    }
}

pub(crate) struct CompactionReplacement {
    stored: StoredCompaction,
    rolling_summary: ModelMessage,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionReplacementErrorReason {
    InvalidRollingSummary,
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct CompactionReplacementError {
    reason: CompactionReplacementErrorReason,
}

#[cfg(test)]
impl CompactionReplacementError {
    const fn invalid_rolling_summary() -> Self {
        Self {
            reason: CompactionReplacementErrorReason::InvalidRollingSummary,
        }
    }
}

#[cfg(test)]
impl fmt::Debug for CompactionReplacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionReplacementError")
            .field("reason", &self.reason)
            .finish()
    }
}

#[cfg(test)]
impl fmt::Display for CompactionReplacementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid compaction replacement")
    }
}

#[cfg(test)]
impl Error for CompactionReplacementError {}

impl CompactionReplacement {
    #[cfg(test)]
    pub(crate) fn for_m4_test(
        stored: StoredCompaction,
    ) -> Result<Self, CompactionReplacementError> {
        let rolling_summary = ModelMessage::rolling_summary(stored.summary.clone())
            .map_err(|_| CompactionReplacementError::invalid_rolling_summary())?;
        Ok(Self {
            stored,
            rolling_summary,
        })
    }

    pub(crate) fn into_parts(self) -> (StoredCompaction, ModelMessage) {
        (self.stored, self.rolling_summary)
    }
}

impl fmt::Debug for CompactionReplacement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CompactionReplacement(<redacted>)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct StoredCompactionModelCall {
    model: ModelResponseSummary,
    response_id: Option<ProviderResponseId>,
    usage: Option<ModelUsage>,
    finish_reason: ModelFinishReason,
    requested_max_output_tokens: NonZeroU32,
    logical_retry_count: u8,
    metadata: ProviderResponseMetadata,
}

impl StoredCompactionModelCall {
    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the frozen Compaction provenance shape"
    )]
    fn new(
        model: ModelResponseSummary,
        response_id: Option<ProviderResponseId>,
        usage: Option<ModelUsage>,
        finish_reason: ModelFinishReason,
        requested_max_output_tokens: NonZeroU32,
        logical_retry_count: u8,
        metadata: ProviderResponseMetadata,
    ) -> Result<Self, CompactionValueError> {
        if !matches!(
            finish_reason,
            ModelFinishReason::Stop | ModelFinishReason::Unknown
        ) {
            return Err(CompactionValueError::FinishReason);
        }
        if logical_retry_count > 1 {
            return Err(CompactionValueError::LogicalRetryCount);
        }
        Ok(Self {
            model,
            response_id,
            usage,
            finish_reason,
            requested_max_output_tokens,
            logical_retry_count,
            metadata,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "fields mirror the frozen Compaction provenance shape"
    )]
    pub(crate) fn reconstruct(
        model: ModelResponseSummary,
        response_id: Option<ProviderResponseId>,
        usage: Option<ModelUsage>,
        finish_reason: ModelFinishReason,
        requested_max_output_tokens: NonZeroU32,
        logical_retry_count: u8,
        metadata: ProviderResponseMetadata,
    ) -> Result<Self, CompactionValueError> {
        Self::new(
            model,
            response_id,
            usage,
            finish_reason,
            requested_max_output_tokens,
            logical_retry_count,
            metadata,
        )
    }

    pub const fn model(&self) -> &ModelResponseSummary {
        &self.model
    }

    pub const fn response_id(&self) -> Option<&ProviderResponseId> {
        self.response_id.as_ref()
    }

    pub const fn usage(&self) -> Option<&ModelUsage> {
        self.usage.as_ref()
    }

    pub const fn finish_reason(&self) -> ModelFinishReason {
        self.finish_reason
    }

    pub const fn requested_max_output_tokens(&self) -> NonZeroU32 {
        self.requested_max_output_tokens
    }

    pub const fn logical_retry_count(&self) -> u8 {
        self.logical_retry_count
    }

    pub const fn metadata(&self) -> &ProviderResponseMetadata {
        &self.metadata
    }
}

impl fmt::Debug for StoredCompactionModelCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCompactionModelCall")
            .field("model", &self.model)
            .field("has_response_id", &self.response_id.is_some())
            .field("has_usage", &self.usage.is_some())
            .field("finish_reason", &self.finish_reason)
            .field(
                "requested_max_output_tokens",
                &self.requested_max_output_tokens,
            )
            .field("logical_retry_count", &self.logical_retry_count)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_gateway::{
        FinalizedAssistantContent, ModelReasoningSummary, ModelServiceClass, ReasoningContent,
        TurnModelSnapshot,
    };
    use crate::prompt::{
        AgentRunCompactionAssemblyBasis, CompactionSummaryAssemblyBasis, ModelAssistantContent,
        ModelAssistantContentRef, ModelMessageRef,
    };
    use crate::tools::{ToolCallId, ToolName, ToolResultContent};
    use crate::wire::BoundedJsonObject;

    fn entry_id(value: &str) -> EntryId {
        value.parse().expect("test entry IDs are valid")
    }

    fn session_id(value: &str) -> SessionId {
        value.parse().expect("test session IDs are valid")
    }

    fn model_message(text: &str) -> ModelMessage {
        ModelMessage::unstamped_user_text(Arc::from(text)).expect("test model messages are valid")
    }

    fn unit(first_entry_id: EntryId, kind: CompactionUnitKind, text: &str) -> LiveCompactionUnit {
        PreparedLiveCompactionUnit::for_live_reducer(kind, Arc::from([model_message(text)]))
            .expect("test unit is valid")
            .bind_origin(first_entry_id)
    }

    fn unit_with_messages(
        first_entry_id: EntryId,
        kind: CompactionUnitKind,
        messages: Vec<ModelMessage>,
    ) -> LiveCompactionUnit {
        PreparedLiveCompactionUnit::for_live_reducer(kind, messages.into())
            .expect("test unit is valid")
            .bind_origin(first_entry_id)
    }

    fn source(session_id: SessionId, units: Arc<[LiveCompactionUnit]>) -> LiveCompactionSourceView {
        source_at_revision(session_id, ConversationRevision::default(), units)
    }

    fn source_at_revision(
        session_id: SessionId,
        revision: ConversationRevision,
        units: Arc<[LiveCompactionUnit]>,
    ) -> LiveCompactionSourceView {
        LiveCompactionSourceView::for_live_reducer(session_id, revision, units)
            .expect("test source is valid")
    }

    fn model_call(
        finish_reason: ModelFinishReason,
        logical_retry_count: u8,
    ) -> Result<StoredCompactionModelCall, CompactionValueError> {
        StoredCompactionModelCall::new(
            ModelResponseSummary::reconstruct(
                "openai".parse().unwrap(),
                "gpt-5-mini".parse().unwrap(),
                ModelReasoningSummary::Disabled,
                ModelServiceClass::Standard,
            ),
            Some("SECRET-RESPONSE-ID".parse().unwrap()),
            Some(ModelUsage::reconstruct(
                Some(1_000),
                Some(100),
                None,
                None,
                None,
                Some(1_100),
                None,
            )),
            finish_reason,
            NonZeroU32::new(512).unwrap(),
            logical_retry_count,
            ProviderResponseMetadata::reconstruct(
                Some("SECRET-PROVIDER-REQUEST-ID".parse().unwrap()),
                Some("SECRET-FINISH".parse().unwrap()),
                Some("SECRET-SERVICE-TIER".parse().unwrap()),
            ),
        )
    }

    fn pressure_model(context_window_tokens: Option<u32>) -> CompactionModelBasis {
        let snapshot = TurnModelSnapshot::test_fixture_with_policy(
            context_window_tokens.and_then(NonZeroU32::new),
            NonZeroU32::new(20),
            NonZeroU32::new(12).unwrap(),
            NonZeroU32::new(1).unwrap(),
        );
        CompactionModelBasis::from_turn_model(&snapshot)
    }

    fn pressure_settings(enabled: bool) -> CompactionSettingsSnapshot {
        CompactionSettings {
            enabled,
            pressure_reserve_tokens: NonZeroU32::new(8).unwrap(),
            summary_min_output_tokens: NonZeroU32::new(5).unwrap(),
            summary_max_output_tokens: NonZeroU32::new(10).unwrap(),
            minimum_reclaimed_tokens: NonZeroU32::new(5).unwrap(),
            max_compactions_per_turn: NonZeroU8::new(2).unwrap(),
            summary_safety_reserve_tokens: NonZeroU32::new(2).unwrap(),
        }
        .validate()
        .unwrap()
    }

    fn pressure_source() -> LiveCompactionSourceView {
        source(
            session_id("ses_11111111111111111111111111111111"),
            Arc::from([unit(
                entry_id("ent_11111111111111111111111111111111"),
                CompactionUnitKind::UserMessage,
                "payload",
            )]),
        )
    }

    #[test]
    fn proactive_pressure_uses_exact_input_and_effective_headroom_thresholds() {
        let source = pressure_source();
        let settings = pressure_settings(true);
        let compaction = Compaction;

        for (context_window, expected) in [
            (61, CompactionPressure::NotNeeded),
            (60, CompactionPressure::Recommended),
            (48, CompactionPressure::Required),
        ] {
            let model = pressure_model(Some(context_window));
            let agent_run = AgentRunCompactionAssemblyBasis::for_test(10, 31, model.estimator());
            assert_eq!(
                compaction.pressure(CompactionPressureInput {
                    source: &source,
                    settings: &settings,
                    agent_run: &agent_run,
                    model: &model,
                    trigger: CompactionTrigger::ProactivePressure,
                    compactions_started: 0,
                }),
                expected,
                "unexpected pressure at context window {context_window}"
            );
        }
    }

    #[test]
    fn hard_overflow_fails_closed_when_compaction_cannot_start() {
        let source = pressure_source();
        let model = pressure_model(Some(60));
        let agent_run = AgentRunCompactionAssemblyBasis::for_test(10, 31, model.estimator());
        let compaction = Compaction;

        for (settings, source, context, count, expected) in [
            (
                pressure_settings(false),
                &source,
                Some(&model),
                0,
                CompactionImpossibleReason::Disabled,
            ),
            (
                pressure_settings(true),
                &LiveCompactionSourceView::for_live_reducer(
                    session_id("ses_11111111111111111111111111111111"),
                    ConversationRevision::default(),
                    Arc::from([]),
                )
                .unwrap(),
                Some(&model),
                0,
                CompactionImpossibleReason::EmptySource,
            ),
            (
                pressure_settings(true),
                &source,
                None,
                0,
                CompactionImpossibleReason::UnknownContextLimit,
            ),
            (
                pressure_settings(true),
                &source,
                Some(&model),
                2,
                CompactionImpossibleReason::CompactionLimitReached,
            ),
        ] {
            let unknown_model;
            let selected_model = match context {
                Some(model) => model,
                None => {
                    unknown_model = pressure_model(None);
                    &unknown_model
                }
            };
            assert_eq!(
                compaction.pressure(CompactionPressureInput {
                    source,
                    settings: &settings,
                    agent_run: &agent_run,
                    model: selected_model,
                    trigger: CompactionTrigger::PromptContextOverflow,
                    compactions_started: count,
                }),
                CompactionPressure::Impossible(expected)
            );
        }
    }

    #[test]
    fn proactive_arithmetic_overflow_obeys_the_per_turn_limit() {
        let source = pressure_source();
        let settings = pressure_settings(true);
        let model = pressure_model(Some(60));
        let agent_run = AgentRunCompactionAssemblyBasis::for_test(u64::MAX, 31, model.estimator());

        assert_eq!(
            Compaction.pressure(CompactionPressureInput {
                source: &source,
                settings: &settings,
                agent_run: &agent_run,
                model: &model,
                trigger: CompactionTrigger::ProactivePressure,
                compactions_started: 2,
            }),
            CompactionPressure::Impossible(CompactionImpossibleReason::CompactionLimitReached)
        );
    }

    fn planning_settings(minimum_reclaimed_tokens: u32) -> CompactionSettingsSnapshot {
        CompactionSettings {
            enabled: true,
            pressure_reserve_tokens: NonZeroU32::new(8).unwrap(),
            summary_min_output_tokens: NonZeroU32::new(20).unwrap(),
            summary_max_output_tokens: NonZeroU32::new(40).unwrap(),
            minimum_reclaimed_tokens: NonZeroU32::new(minimum_reclaimed_tokens).unwrap(),
            max_compactions_per_turn: NonZeroU8::new(2).unwrap(),
            summary_safety_reserve_tokens: NonZeroU32::new(5).unwrap(),
        }
        .validate()
        .unwrap()
    }

    fn planning_model() -> CompactionModelBasis {
        let snapshot = TurnModelSnapshot::test_fixture_with_policy(
            NonZeroU32::new(650),
            NonZeroU32::new(50),
            NonZeroU32::new(12).unwrap(),
            NonZeroU32::new(1).unwrap(),
        );
        CompactionModelBasis::from_turn_model(&snapshot)
    }

    fn planning_source() -> Arc<LiveCompactionSourceView> {
        Arc::new(source(
            session_id("ses_11111111111111111111111111111111"),
            Arc::from([
                unit(
                    entry_id("ent_11111111111111111111111111111111"),
                    CompactionUnitKind::UserMessage,
                    &"a".repeat(80),
                ),
                unit(
                    entry_id("ent_22222222222222222222222222222222"),
                    CompactionUnitKind::AssistantMessage,
                    &"b".repeat(80),
                ),
                unit(
                    entry_id("ent_33333333333333333333333333333333"),
                    CompactionUnitKind::UserMessage,
                    &"c".repeat(80),
                ),
            ]),
        ))
    }

    fn plan_input(minimum_reclaimed_tokens: u32) -> CompactionPlanInput {
        let model = planning_model();
        CompactionPlanInput {
            source: planning_source(),
            settings: planning_settings(minimum_reclaimed_tokens),
            agent_run: AgentRunCompactionAssemblyBasis::for_test(10, 31, model.estimator()),
            summary_assembly: CompactionSummaryAssemblyBasis::for_test(20, model.estimator()),
            model,
            trigger: CompactionTrigger::ProviderContextOverflow,
            compactions_started: 0,
        }
    }

    #[test]
    fn plan_selects_first_feasible_stable_unit_prefix_and_derives_marker() {
        let input = plan_input(100);
        let source = Arc::clone(&input.source);
        let plan = Compaction.plan(input).unwrap();

        assert!(Arc::ptr_eq(plan.source(), &source));
        assert_eq!(plan.summary_source().source_revision(), *source.revision());
        assert_eq!(plan.summarized_unit_count().get(), 2);
        assert_eq!(plan.summary_source().messages().len(), 2);
        assert_eq!(plan.retained_units().len(), 1);
        assert_eq!(
            plan.first_kept_entry_id(),
            Some(entry_id("ent_33333333333333333333333333333333"))
        );
        assert_eq!(plan.budget().max_output_tokens().get(), 40);
        assert_eq!(plan.estimated_before_tokens(), 343);
        assert_eq!(plan.estimated_after_upper_bound_tokens(), 192);
        assert_eq!(plan.estimated_reclaimed_tokens(), 151);
    }

    #[test]
    fn plan_deterministically_reduces_large_tool_results_only_in_summary_source() {
        let tool_call_id: ToolCallId = "call_large_result".parse().unwrap();
        let tool_name: ToolName = "inspect".parse().unwrap();
        let large_result = vec![
            format!("HEAD:{}", "x".repeat(49_995)),
            "x".repeat(50_000),
            "x".repeat(50_000),
            format!("{}:TAIL", "x".repeat(49_995)),
        ];
        let original_bytes = large_result.iter().map(String::len).sum::<usize>();
        let assistant = ModelMessage::assistant(Arc::from([ModelAssistantContent::tool_call(
            tool_call_id.clone(),
            tool_name,
            BoundedJsonObject::from_slice(br#"{}"#).unwrap(),
        )]))
        .unwrap();
        let tool = ModelMessage::tool_result(
            tool_call_id.clone(),
            ToolResultContent::from_text_parts(large_result.clone()).unwrap(),
        );
        let source = Arc::new(source(
            session_id("ses_11111111111111111111111111111111"),
            Arc::from([
                unit_with_messages(
                    entry_id("ent_11111111111111111111111111111111"),
                    CompactionUnitKind::ToolExchange,
                    vec![assistant, tool],
                ),
                unit(
                    entry_id("ent_22222222222222222222222222222222"),
                    CompactionUnitKind::UserMessage,
                    "retained suffix",
                ),
            ]),
        ));
        let snapshot = TurnModelSnapshot::test_fixture_with_policy(
            NonZeroU32::new(50_000),
            NonZeroU32::new(512),
            NonZeroU32::new(12).unwrap(),
            NonZeroU32::new(1).unwrap(),
        );
        let model = CompactionModelBasis::from_turn_model(&snapshot);
        let input = || CompactionPlanInput {
            source: Arc::clone(&source),
            settings: CompactionSettings {
                enabled: true,
                pressure_reserve_tokens: NonZeroU32::new(512).unwrap(),
                summary_min_output_tokens: NonZeroU32::new(128).unwrap(),
                summary_max_output_tokens: NonZeroU32::new(512).unwrap(),
                minimum_reclaimed_tokens: NonZeroU32::new(1).unwrap(),
                max_compactions_per_turn: NonZeroU8::new(2).unwrap(),
                summary_safety_reserve_tokens: NonZeroU32::new(128).unwrap(),
            }
            .validate()
            .unwrap(),
            agent_run: AgentRunCompactionAssemblyBasis::for_test(10, 31, model.estimator()),
            summary_assembly: CompactionSummaryAssemblyBasis::for_test(20, model.estimator()),
            model: model.clone(),
            trigger: CompactionTrigger::ProviderContextOverflow,
            compactions_started: 0,
        };

        let first = Compaction.plan(input()).unwrap();
        let second = Compaction.plan(input()).unwrap();
        assert_eq!(first.summarized_unit_count().get(), 1);
        assert_eq!(first.summary_source().messages().len(), 2);
        assert_eq!(
            first.budget().reduced_source_tokens(),
            second.budget().reduced_source_tokens()
        );
        assert!(
            first
                .summary_source()
                .messages()
                .iter()
                .zip(second.summary_source().messages())
                .all(|(left, right)| left.as_ref() == right.as_ref())
        );

        match first.summary_source().messages()[0].as_ref() {
            ModelMessageRef::Assistant { content } => match content[0].as_ref() {
                ModelAssistantContentRef::ToolCall {
                    tool_call_id: reduced_id,
                    name,
                    ..
                } => {
                    assert_eq!(reduced_id, &tool_call_id);
                    assert_eq!(name.as_str(), "inspect");
                }
                _ => panic!("the reduced exchange must retain the original ToolCall"),
            },
            _ => panic!("the reduced exchange must retain the Assistant message"),
        }
        match first.summary_source().messages()[1].as_ref() {
            ModelMessageRef::Tool {
                tool_call_id: reduced_id,
                content,
            } => {
                assert_eq!(reduced_id, &tool_call_id);
                assert_eq!(content.parts().len(), 3);
                let header = content.parts()[0].as_text();
                assert!(header.contains("format_version=1"));
                assert!(header.contains(&format!("original_bytes={original_bytes}")));
                assert!(header.contains("omitted_bytes="));
                assert!(content.parts()[1].as_text().contains("HEAD:"));
                assert!(content.parts()[2].as_text().ends_with(":TAIL"));
            }
            _ => panic!("the reduced exchange must retain a Tool result message"),
        }

        match source.units()[0].messages()[1].as_ref() {
            ModelMessageRef::Tool { content, .. } => {
                assert_eq!(content.parts().len(), 4);
                assert_eq!(
                    content
                        .parts()
                        .iter()
                        .map(|part| part.as_text().to_owned())
                        .collect::<Vec<_>>(),
                    large_result
                );
            }
            _ => panic!("the live source must remain unchanged"),
        }
    }

    #[test]
    fn tool_result_reduction_threshold_and_utf8_boundaries_are_stable() {
        let tool_call_id: ToolCallId = "call_threshold".parse().unwrap();
        let at_threshold = ModelMessage::tool_result(
            tool_call_id.clone(),
            ToolResultContent::from_text_parts(vec![
                "x".repeat(COMPACTION_TOOL_RESULT_REDUCTION_THRESHOLD_BYTES),
            ])
            .unwrap(),
        );
        let unchanged = reduce_summary_message(&at_threshold).unwrap();
        assert_eq!(unchanged.as_ref(), at_threshold.as_ref());

        let unicode = "é".repeat(
            COMPACTION_TOOL_RESULT_REDUCTION_THRESHOLD_BYTES
                .checked_div("é".len())
                .unwrap()
                + 1,
        );
        let expected_omitted = unicode.len()
            - COMPACTION_TOOL_RESULT_REDUCTION_HEAD_BYTES
            - COMPACTION_TOOL_RESULT_REDUCTION_TAIL_BYTES;
        let above_threshold = ModelMessage::tool_result(
            tool_call_id,
            ToolResultContent::from_text_parts(vec![unicode]).unwrap(),
        );
        let reduced = reduce_summary_message(&above_threshold).unwrap();
        match reduced.as_ref() {
            ModelMessageRef::Tool { content, .. } => {
                assert_eq!(content.parts().len(), 3);
                assert!(content.parts()[1].as_text().ends_with('é'));
                assert!(content.parts()[2].as_text().ends_with('é'));
                assert!(
                    content.parts()[0]
                        .as_text()
                        .contains(&format!("omitted_bytes={expected_omitted}"))
                );
            }
            _ => panic!("a reduced Tool result must retain the Tool role"),
        }

        let split_boundaries = ModelMessage::tool_result(
            "call_split_boundary".parse().unwrap(),
            ToolResultContent::from_text_parts(vec![
                format!("{}🦀", "h".repeat(4_095)),
                "middle-left".repeat(1_000),
                "middle-right".repeat(1_000),
                format!("🦀{}", "t".repeat(4_095)),
            ])
            .unwrap(),
        );
        let split_reduced = reduce_summary_message(&split_boundaries).unwrap();
        match split_reduced.as_ref() {
            ModelMessageRef::Tool { content, .. } => {
                assert!(!content.parts()[1].as_text().contains("[part=1]"));
                assert!(!content.parts()[2].as_text().contains("[part=2]"));
            }
            _ => panic!("a reduced Tool result must retain the Tool role"),
        }
    }

    #[test]
    fn plan_all_units_boundary_derives_none_marker() {
        let plan = Compaction.plan(plan_input(200)).unwrap();

        assert_eq!(plan.summarized_unit_count().get(), 3);
        assert!(plan.retained_units().is_empty());
        assert_eq!(plan.first_kept_entry_id(), None);
        assert_eq!(plan.budget().max_output_tokens().get(), 40);
    }

    #[test]
    fn plan_reports_checked_arithmetic_overflow_instead_of_clamping() {
        let mut input = plan_input(100);
        input.agent_run =
            AgentRunCompactionAssemblyBasis::for_test(u64::MAX, 31, input.model.estimator());

        assert_eq!(
            Compaction.plan(input).unwrap_err().reason(),
            CompactionErrorReason::ArithmeticOverflow
        );
    }

    #[test]
    fn plan_intersects_configured_and_model_summary_output_limits() {
        let mut input = plan_input(100);
        let snapshot = TurnModelSnapshot::test_fixture_with_policy(
            NonZeroU32::new(650),
            NonZeroU32::new(30),
            NonZeroU32::new(12).unwrap(),
            NonZeroU32::new(1).unwrap(),
        );
        input.model = CompactionModelBasis::from_turn_model(&snapshot);

        let plan = Compaction.plan(input).unwrap();
        assert_eq!(plan.summarized_unit_count().get(), 2);
        assert_eq!(plan.budget().max_output_tokens().get(), 30);
        assert_eq!(plan.estimated_after_upper_bound_tokens(), 182);
        assert_eq!(plan.estimated_reclaimed_tokens(), 161);
    }

    #[test]
    fn plan_distinguishes_summary_budget_and_minimum_reclaim_failures() {
        let reclaim_error = Compaction.plan(plan_input(300)).unwrap_err();
        assert_eq!(
            reclaim_error.reason(),
            CompactionErrorReason::InsufficientReclaim
        );

        let mut budget_input = plan_input(100);
        let snapshot = TurnModelSnapshot::test_fixture_with_policy(
            NonZeroU32::new(650),
            NonZeroU32::new(10),
            NonZeroU32::new(10).unwrap(),
            NonZeroU32::new(1).unwrap(),
        );
        budget_input.model = CompactionModelBasis::from_turn_model(&snapshot);
        let budget_error = Compaction.plan(budget_input).unwrap_err();
        assert_eq!(
            budget_error.reason(),
            CompactionErrorReason::NoFeasibleSummaryBudget
        );
    }

    #[test]
    fn plan_rejects_cross_estimator_basis_composition() {
        let mut input = plan_input(100);
        let other = TurnModelSnapshot::test_fixture_with_policy(
            NonZeroU32::new(650),
            NonZeroU32::new(50),
            NonZeroU32::new(12).unwrap(),
            NonZeroU32::new(2).unwrap(),
        );
        input.agent_run =
            AgentRunCompactionAssemblyBasis::for_test(10, 31, other.token_estimator());

        assert_eq!(
            Compaction.plan(input).unwrap_err().reason(),
            CompactionErrorReason::MismatchedEstimator
        );
    }

    fn summary_result(
        plan: &CompactionPlan,
        content: Arc<[FinalizedAssistantContent]>,
        finish_reason: ModelFinishReason,
    ) -> ModelCallResult {
        ModelCallResult::for_compaction_test(
            plan.model().model_summary().clone(),
            content,
            finish_reason,
            plan.budget().max_output_tokens(),
        )
    }

    #[test]
    fn summary_validation_rejects_wrong_model_finish_content_budget_and_retry_facts() {
        let plan = Compaction.plan(plan_input(100)).unwrap();
        let text = || {
            Arc::from([FinalizedAssistantContent::Text {
                text: Arc::from("portable summary"),
            }])
        };
        let wrong_model = ModelCallResult::for_compaction_test(
            ModelResponseSummary::reconstruct(
                "other".parse().unwrap(),
                "other".parse().unwrap(),
                ModelReasoningSummary::Disabled,
                ModelServiceClass::Standard,
            ),
            text(),
            ModelFinishReason::Stop,
            plan.budget().max_output_tokens(),
        );
        let refused = summary_result(&plan, text(), ModelFinishReason::Refused);
        let empty = summary_result(
            &plan,
            Arc::from([FinalizedAssistantContent::Text {
                text: Arc::from(""),
            }]),
            ModelFinishReason::Stop,
        );
        let multiple = summary_result(
            &plan,
            Arc::from([
                FinalizedAssistantContent::Text {
                    text: Arc::from("one"),
                },
                FinalizedAssistantContent::Text {
                    text: Arc::from("two"),
                },
            ]),
            ModelFinishReason::Stop,
        );
        let reasoning_only = summary_result(
            &plan,
            Arc::from([FinalizedAssistantContent::Reasoning(
                ReasoningContent::reconstruct(Some("reasoning".to_owned()), None, None, None, None)
                    .unwrap(),
            )]),
            ModelFinishReason::Unknown,
        );
        let unsafe_text = summary_result(
            &plan,
            Arc::from([FinalizedAssistantContent::Text {
                text: Arc::from("unsafe\r\nsummary"),
            }]),
            ModelFinishReason::Stop,
        );
        let wrong_budget = ModelCallResult::for_compaction_test(
            plan.model().model_summary().clone(),
            text(),
            ModelFinishReason::Stop,
            NonZeroU32::new(plan.budget().max_output_tokens().get() - 1).unwrap(),
        );

        for (result, retry_count) in [
            (wrong_model, 0),
            (refused, 0),
            (empty, 0),
            (multiple, 0),
            (reasoning_only, 0),
            (unsafe_text, 0),
            (wrong_budget, 0),
            (summary_result(&plan, text(), ModelFinishReason::Stop), 2),
        ] {
            assert_eq!(
                Compaction
                    .validate_summary(Arc::clone(&plan), &result, retry_count)
                    .unwrap_err()
                    .reason(),
                CompactionErrorReason::InvalidSummary
            );
        }
    }

    #[test]
    fn summary_validation_ignores_optional_reasoning_but_preserves_text_verbatim() {
        let plan = Compaction.plan(plan_input(100)).unwrap();
        let result = summary_result(
            &plan,
            Arc::from([
                FinalizedAssistantContent::Reasoning(
                    ReasoningContent::reconstruct(
                        None,
                        Some("portable reasoning summary".to_owned()),
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                ),
                FinalizedAssistantContent::Text {
                    text: Arc::from("portable\nsummary"),
                },
            ]),
            ModelFinishReason::Unknown,
        );

        let validated = Compaction
            .validate_summary(Arc::clone(&plan), &result, 0)
            .unwrap();
        assert!(Arc::ptr_eq(validated.plan(), &plan));
        let debug = format!("{validated:?}");
        assert!(!debug.contains("portable reasoning summary"));
        assert!(!debug.contains("portable\nsummary"));
        let (stored, _) = validated.into_replacement().unwrap().into_parts();
        assert_eq!(stored.summary(), "portable\nsummary");
        assert!(stored.model_call().is_some());
    }

    #[test]
    fn compaction_settings_defaults_validate_and_snapshot_without_drift() {
        let defaults = CompactionSettings::default();
        assert!(defaults.enabled);
        assert_eq!(defaults.pressure_reserve_tokens.get(), 4_096);
        assert_eq!(defaults.summary_min_output_tokens.get(), 512);
        assert_eq!(defaults.summary_max_output_tokens.get(), 2_048);
        assert_eq!(defaults.minimum_reclaimed_tokens.get(), 2_048);
        assert_eq!(defaults.max_compactions_per_turn.get(), 4);
        assert_eq!(defaults.summary_safety_reserve_tokens.get(), 512);

        let snapshot = defaults.validate().unwrap();
        let cloned = snapshot.clone();
        assert_eq!(snapshot.summary_min_output_tokens().get(), 512);
        assert_eq!(cloned.summary_max_output_tokens().get(), 2_048);
        assert!(Arc::ptr_eq(&snapshot.0, &cloned.0));
    }

    #[test]
    fn compaction_settings_reject_inverted_summary_output_range() {
        let settings = CompactionSettings {
            summary_min_output_tokens: NonZeroU32::new(2_049).unwrap(),
            ..CompactionSettings::default()
        };

        assert_eq!(
            settings.validate(),
            Err(CompactionSettingsError::InvalidSummaryOutputRange)
        );
    }

    #[test]
    fn stored_summary_is_safe_and_bounded_by_utf8_bytes() {
        let stored = StoredCompaction::reconstruct("a\nb", None, None).unwrap();
        assert_eq!(stored.summary(), "a\nb");
        assert!(StoredCompaction::reconstruct("", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\u{001b}", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\r\ntext", None, None).is_err());
        assert!(StoredCompaction::reconstruct("bad\rtext", None, None).is_err());
        assert!(
            StoredCompaction::reconstruct(
                "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES),
                None,
                None,
            )
            .is_ok()
        );
        assert!(
            StoredCompaction::reconstruct(
                "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES + 1),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            StoredCompaction::reconstruct(
                "é".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES / 2),
                None,
                None,
            )
            .is_ok()
        );
        assert!(
            StoredCompaction::reconstruct(
                "é".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES / 2 + 1),
                None,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn stored_model_call_accepts_only_portable_finish_and_retry_facts() {
        assert!(model_call(ModelFinishReason::Stop, 0).is_ok());
        assert!(model_call(ModelFinishReason::Unknown, 1).is_ok());
        for finish_reason in [
            ModelFinishReason::ToolCalls,
            ModelFinishReason::Length,
            ModelFinishReason::ContentFiltered,
            ModelFinishReason::Refused,
        ] {
            assert_eq!(
                model_call(finish_reason, 0),
                Err(CompactionValueError::FinishReason)
            );
        }
        assert_eq!(
            model_call(ModelFinishReason::Stop, 2),
            Err(CompactionValueError::LogicalRetryCount)
        );
    }

    #[test]
    fn stored_compaction_reconstructs_with_or_without_model_call() {
        let marker: EntryId = "ent_11111111111111111111111111111111".parse().unwrap();
        let automatic = StoredCompaction::reconstruct(
            "summary",
            Some(marker),
            Some(model_call(ModelFinishReason::Stop, 1).unwrap()),
        )
        .unwrap();
        assert_eq!(automatic.first_kept_entry_id(), Some(marker));
        assert!(automatic.model_call().is_some());
        assert_eq!(
            automatic
                .model_call()
                .unwrap()
                .requested_max_output_tokens()
                .get(),
            512
        );

        let replayed = StoredCompaction::reconstruct("summary", Some(marker), None).unwrap();
        assert_eq!(replayed.first_kept_entry_id(), Some(marker));
        assert!(replayed.model_call().is_none());
    }

    #[test]
    fn compaction_debug_does_not_expose_summary_or_provider_ids() {
        let stored = StoredCompaction::reconstruct(
            "SECRET-SUMMARY",
            None,
            Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
        )
        .unwrap();
        let debug = format!("{stored:?} {:?}", stored.model_call().unwrap());
        assert!(!debug.contains("SECRET-SUMMARY"));
        assert!(!debug.contains("SECRET-RESPONSE-ID"));
        assert!(!debug.contains("SECRET-PROVIDER-REQUEST-ID"));
        assert!(!debug.contains("SECRET-FINISH"));
        assert!(!debug.contains("SECRET-SERVICE-TIER"));
    }

    #[test]
    fn stable_unit_kinds_are_closed_and_distinct() {
        let kinds = [
            CompactionUnitKind::RollingSummary,
            CompactionUnitKind::UserMessage,
            CompactionUnitKind::AssistantMessage,
            CompactionUnitKind::ToolExchange,
        ];
        assert_eq!(kinds.len(), 4);
        for (index, kind) in kinds.iter().enumerate() {
            assert!(!kinds[..index].contains(kind));
        }
    }

    #[test]
    fn live_compaction_unit_and_source_factories_validate_structural_invariants() {
        let empty: Arc<[ModelMessage]> = Arc::from([]);
        let Err(error) =
            PreparedLiveCompactionUnit::for_live_reducer(CompactionUnitKind::UserMessage, empty)
        else {
            panic!("empty unit messages unexpectedly succeeded");
        };
        assert_eq!(error.reason, CompactionSourceErrorReason::EmptyUnitMessages);

        let duplicate = entry_id("ent_11111111111111111111111111111111");
        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([
                unit(duplicate, CompactionUnitKind::UserMessage, "SECRET-FIRST"),
                unit(
                    duplicate,
                    CompactionUnitKind::AssistantMessage,
                    "SECRET-SECOND",
                ),
            ]),
        ) else {
            panic!("duplicate unit origin unexpectedly succeeded");
        };
        assert_eq!(
            error.reason,
            CompactionSourceErrorReason::DuplicateUnitOrigin
        );
        for output in [format!("{error:?}"), error.to_string()] {
            assert!(!output.contains("SECRET-FIRST"));
            assert!(!output.contains("SECRET-SECOND"));
            assert!(!output.contains(&duplicate.to_string()));
        }
        assert!(std::error::Error::source(&error).is_none());

        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([
                unit(
                    entry_id("ent_22222222222222222222222222222222"),
                    CompactionUnitKind::UserMessage,
                    "ordinary",
                ),
                unit(
                    entry_id("ent_33333333333333333333333333333333"),
                    CompactionUnitKind::RollingSummary,
                    "summary",
                ),
            ]),
        ) else {
            panic!("misplaced rolling summary unexpectedly succeeded");
        };
        assert_eq!(
            error.reason,
            CompactionSourceErrorReason::MisplacedRollingSummary
        );
    }

    #[test]
    fn live_compaction_source_factory_rejects_forged_empty_unit_messages() {
        let forged_origin = entry_id("ent_11111111111111111111111111111111");
        let forged = LiveCompactionUnit {
            first_entry_id: forged_origin,
            kind: CompactionUnitKind::UserMessage,
            messages: Arc::from([]),
        };

        let Err(error) = LiveCompactionSourceView::for_live_reducer(
            session_id("ses_11111111111111111111111111111111"),
            ConversationRevision::default(),
            Arc::from([forged]),
        ) else {
            panic!("forged empty unit messages unexpectedly succeeded");
        };

        assert_eq!(error.reason, CompactionSourceErrorReason::EmptyUnitMessages);
        for output in [format!("{error:?}"), error.to_string()] {
            assert!(!output.contains(&forged_origin.to_string()));
        }
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn stable_identity_uses_session_revision_and_unit_origins_not_message_values() {
        let session = session_id("ses_11111111111111111111111111111111");
        let first = entry_id("ent_11111111111111111111111111111111");
        let second = entry_id("ent_22222222222222222222222222222222");
        let third = entry_id("ent_33333333333333333333333333333333");
        let original = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let changed_messages = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "different user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "different assistant",
                ),
            ]),
        );
        let other_revision = source_at_revision(
            session,
            ConversationRevision::default().checked_next().unwrap(),
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let fewer_units = source(
            session,
            Arc::from([unit(
                first,
                CompactionUnitKind::UserMessage,
                "original user",
            )]),
        );
        let reordered_units = source(
            session,
            Arc::from([
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
                unit(first, CompactionUnitKind::UserMessage, "original user"),
            ]),
        );
        let changed_first_entry_id = source(
            session,
            Arc::from([
                unit(third, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );
        let changed_kind = source(
            session,
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::ToolExchange,
                    "original assistant",
                ),
            ]),
        );
        let other_session = source(
            session_id("ses_22222222222222222222222222222222"),
            Arc::from([
                unit(first, CompactionUnitKind::UserMessage, "original user"),
                unit(
                    second,
                    CompactionUnitKind::AssistantMessage,
                    "original assistant",
                ),
            ]),
        );

        assert!(original.has_same_stable_identity(&changed_messages));
        assert!(!original.has_same_stable_identity(&other_revision));
        assert!(!original.has_same_stable_identity(&fewer_units));
        assert!(!original.has_same_stable_identity(&reordered_units));
        assert!(!original.has_same_stable_identity(&changed_first_entry_id));
        assert!(!original.has_same_stable_identity(&changed_kind));
        assert!(!original.has_same_stable_identity(&other_session));
        assert_eq!(original.session_id(), &session);
        assert_eq!(original.revision(), &ConversationRevision::default());
        assert_eq!(original.units()[0].first_entry_id(), &first);
        assert_eq!(original.units()[0].kind(), CompactionUnitKind::UserMessage);
    }

    #[test]
    fn compaction_source_clone_and_origin_binding_preserve_arc_backed_values() {
        let messages: Arc<[ModelMessage]> = Arc::from([model_message("arc preserved")]);
        let prepared = PreparedLiveCompactionUnit::for_live_reducer(
            CompactionUnitKind::UserMessage,
            messages.clone(),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&prepared.messages, &messages));

        let unit = prepared.bind_origin(entry_id("ent_11111111111111111111111111111111"));
        assert!(Arc::ptr_eq(&unit.messages, &messages));
        assert!(std::ptr::eq(&unit.messages()[0], &messages[0]));
        let unit_clone = unit.clone();
        assert!(Arc::ptr_eq(&unit.messages, &unit_clone.messages));
        assert!(std::ptr::eq(&unit.messages()[0], &unit_clone.messages()[0],));

        let source = source(
            session_id("ses_11111111111111111111111111111111"),
            Arc::from([unit]),
        );
        let clone = source.clone();
        assert!(Arc::ptr_eq(&source.units, &clone.units));
        assert!(std::ptr::eq(
            &source.units()[0].messages()[0],
            &clone.units()[0].messages()[0],
        ));
    }

    #[test]
    fn m4_replacement_is_consuming_and_redacts_summary_validation_details() {
        let marker = entry_id("ent_11111111111111111111111111111111");
        let stored = StoredCompaction::reconstruct(
            "SECRET-ROLLING-SUMMARY",
            Some(marker),
            Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
        )
        .unwrap();
        let replacement = CompactionReplacement::for_m4_test(stored.clone()).unwrap();
        let debug = format!("{replacement:?}");
        assert_eq!(debug, "CompactionReplacement(<redacted>)");
        assert!(!debug.contains("SECRET-ROLLING-SUMMARY"));
        assert!(!debug.contains(&marker.to_string()));
        assert!(!debug.contains("SECRET-RESPONSE-ID"));
        assert!(!debug.contains("SECRET-PROVIDER-REQUEST-ID"));

        let (returned_stored, rolling_summary) = replacement.into_parts();
        assert_eq!(returned_stored, stored);
        assert!(Arc::ptr_eq(&returned_stored.summary, &stored.summary));
        let ModelMessageRef::User { content } = rolling_summary.as_ref() else {
            panic!("replacement did not materialize a user-role rolling summary");
        };
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), "SECRET-ROLLING-SUMMARY");
        assert_eq!(
            content[0].as_text().as_ptr(),
            returned_stored.summary().as_ptr()
        );

        let too_long_summary: Arc<str> = format!(
            "SECRET-TOO-LONG-{}",
            "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES + 1 - "SECRET-TOO-LONG-".len())
        )
        .into();
        assert_eq!(
            too_long_summary.len(),
            MAX_STORED_COMPACTION_SUMMARY_BYTES + 1
        );
        let forged_summaries: [(&str, Arc<str>); 3] = [
            ("EmptyText", Arc::from("")),
            ("UnsafeText", Arc::from("SECRET-UNSAFE\r\nSUMMARY")),
            ("TextTooLong", too_long_summary),
        ];

        for (case, summary) in forged_summaries {
            let Err(error) = CompactionReplacement::for_m4_test(StoredCompaction {
                summary,
                first_kept_entry_id: Some(marker),
                model_call: Some(model_call(ModelFinishReason::Stop, 0).unwrap()),
            }) else {
                panic!("{case} rolling summary unexpectedly succeeded");
            };
            assert_eq!(
                error.reason,
                CompactionReplacementErrorReason::InvalidRollingSummary,
                "{case} rolling summary mapped to the wrong error"
            );
            assert_eq!(
                format!("{error:?}"),
                "CompactionReplacementError { reason: InvalidRollingSummary }",
                "{case} rolling summary debug output leaked validation details"
            );
            assert_eq!(
                error.to_string(),
                "invalid compaction replacement",
                "{case} rolling summary display output leaked validation details"
            );
            assert!(
                std::error::Error::source(&error).is_none(),
                "{case} rolling summary unexpectedly retained a source error"
            );
        }
    }
}
