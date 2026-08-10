#![allow(
    dead_code,
    reason = "the loaded Session executor awaits public routing and Turn integration"
)]

//! The crate-private loaded Session execution seam.
//!
//! This module deliberately stops at one already-loaded, Idle Session (Ready or Unavailable).
//! It retains the
//! replay-seeded live state and inline Recorder supplied by residency, but owns neither Runtime
//! residency nor the public Runtime facade. The Runtime-owned residency registry that starts an
//! executor retains its permit (and excludes lifecycle/load changes) for as long as the loaded
//! executor is live; this constructor does not acquire that permit.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent_session_lifecycle::{
    AgentRevisionRef, ForkAnchor, SealedSessionAgentUpgradeAttempt, SealedSessionDefinitionAttempt,
    SessionDefinition, SessionDefinitionDecision, SessionDefinitionDecisionError, SessionLifecycle,
    SessionMetadata, SessionModelConfig,
};
use crate::compaction::{
    Compaction, CompactionPlan, CompactionPressure, CompactionSettings, CompactionSettingsSnapshot,
    CompactionTrigger,
};
use crate::conversation_storage::{
    CapturedForkConversation, ConversationReplayDiagnostics, ForkAnchorResolutionError,
    RecordingHealth, SessionRecorder, SessionRecordingError, StoredAssistantContent,
    StoredAssistantMessage, StoredEntryBody, StoredToolMessage, StoredToolOutcome,
    StoredUserMessage,
};
use crate::durable_state::{
    AgentAdmissionError, DurableAgentDefinitionReadError, DurableSessionAgentUpgradeError,
    DurableSessionAgentUpgradeOutcome, DurableSessionDefinitionError,
    DurableSessionDefinitionOutcome, DurableState,
};
use crate::live_conversation::{
    HostInteractionResolutionApplyOutcome, HostInteractionResolutionError,
    InteractionRequestCandidate, InteractionResolutionApplyOutcome, InteractionResolutionCandidate,
    LiveSessionState,
};
use crate::model_gateway::{
    FinalizedAssistantContent, ModelCallError, ModelCallErrorReason, ModelCallPurpose,
    ModelCallRequest, ModelCallResult, ModelCatalogView, ModelGateway, ModelProgressPublisher,
    ModelRequestValidationErrorKind, ModelResolutionError, ModelResolutionErrorKind, ModelUsage,
    ProviderRequestDeliveryState, ResolveTurnModelRequest,
};
use crate::prompt::{
    PromptError, PromptErrorKind, PromptIntent, PromptResourceView, PromptService,
    SessionPromptSelection,
};
use crate::runtime_interface::{
    CurrentTurnView, InteractionView, ItemContentView, ItemStatusView, ItemView,
    SessionDiagnosticView, SessionReadinessView, SessionRecordingState, SessionUnavailableView,
    SessionUsageView, TurnExecutionPhaseView, TurnStatusView,
};
use crate::runtime_task::{Clock, RuntimeTaskContext, RuntimeTaskError, SystemClock, TrackedTask};
use crate::session_ingress::{
    EmergencyControlHandle, EmergencyControlObservation, EmergencyControlSignal,
    EmergencyControlSignalOutcome, EmergencyControlTarget, FollowUpQueue, FollowUpQueueError,
    QueuedSteer, SteerQueue, SteerQueueError,
};
#[cfg(test)]
use crate::tools::ToolExecutionResult;
use crate::tools::{ToolCall, ToolExecutionOutcome, ToolExecutionRequest, ToolSet};
use crate::turn_execution_context::{
    TurnContextCapture, TurnContextCaptureError, TurnExecutionContext,
};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, InteractionRequest, InteractionRequestView,
    InteractionResolutionInput, ResolvedInteraction, UserMessageSource,
};
use crate::wire::{
    CommandId, CurrencyCode, IdGenerationError, InteractionResolutionKey, ItemId, Money,
    MoneyAmount, ProtocolLimits, RequestId, SessionDefinitionRevision, SessionId, Timestamp,
    TurnId, WorkspaceRevision,
};
use crate::workspace::{
    Workspace, WorkspaceResolveError, WorkspaceResolver, WorkspaceSnapshot,
    WorkspaceSnapshotFinishError,
};

const SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY: usize = 8;
const SESSION_EVENT_CAPACITY: usize = 32;
const AGENT_RUN_MAX_LOGICAL_RETRIES: usize = 3;
const AGENT_RUN_RETRY_BACKOFFS: [std::time::Duration; AGENT_RUN_MAX_LOGICAL_RETRIES] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(4),
    std::time::Duration::from_secs(8),
];
const COMPACTION_SUMMARY_MAX_LOGICAL_RETRIES: u8 = 1;

/// The only execution states represented by a loaded Session executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionExecutionState {
    Idle,
    Starting,
    Running,
    Finishing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTurnFailure {
    Prompt,
    Model,
    ContextOverflow,
    AgentUnavailable,
    Internal,
    EmergencyControl(EmergencyControlSignal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTurnInterruption {
    UserCancelled,
    SecurityRevoked,
    PrepareForUnload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionTurnTerminal {
    Completed,
    Interrupted(SessionTurnInterruption),
    Failed(SessionTurnFailure),
}

#[derive(Clone)]
pub(crate) enum SessionExecutorEvent {
    ExecutionChanged {
        timestamp: Timestamp,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
    DefinitionUpdated {
        timestamp: Timestamp,
        command_id: CommandId,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
    MetadataUpdated {
        timestamp: Timestamp,
        command_id: CommandId,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
    WorkspaceReloaded {
        timestamp: Timestamp,
        command_id: CommandId,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
    ReadinessChanged {
        timestamp: Timestamp,
        // Agent/shared-resource reload sources carry their owning CommandId; the host security
        // invalidation seam publishes the same event with `None` (no CommandId is generated).
        command_id: Option<CommandId>,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
    TurnTerminal {
        timestamp: Timestamp,
        command_id: CommandId,
        turn_id: TurnId,
        terminal: SessionTurnTerminal,
        snapshot: Arc<SessionExecutorSnapshot>,
    },
}

impl SessionExecutorEvent {
    pub(crate) const fn timestamp(&self) -> Timestamp {
        match self {
            Self::ExecutionChanged { timestamp, .. }
            | Self::DefinitionUpdated { timestamp, .. }
            | Self::MetadataUpdated { timestamp, .. }
            | Self::WorkspaceReloaded { timestamp, .. }
            | Self::ReadinessChanged { timestamp, .. }
            | Self::TurnTerminal { timestamp, .. } => *timestamp,
        }
    }

    pub(crate) const fn command_id(&self) -> Option<CommandId> {
        match self {
            Self::ExecutionChanged { .. } => None,
            Self::DefinitionUpdated { command_id, .. }
            | Self::MetadataUpdated { command_id, .. }
            | Self::WorkspaceReloaded { command_id, .. }
            | Self::TurnTerminal { command_id, .. } => Some(*command_id),
            Self::ReadinessChanged { command_id, .. } => *command_id,
        }
    }

    pub(crate) const fn turn_id(&self) -> Option<TurnId> {
        match self {
            Self::ExecutionChanged { .. }
            | Self::DefinitionUpdated { .. }
            | Self::MetadataUpdated { .. }
            | Self::WorkspaceReloaded { .. }
            | Self::ReadinessChanged { .. } => None,
            Self::TurnTerminal { turn_id, .. } => Some(*turn_id),
        }
    }

    pub(crate) const fn terminal(&self) -> Option<SessionTurnTerminal> {
        match self {
            Self::ExecutionChanged { .. }
            | Self::DefinitionUpdated { .. }
            | Self::MetadataUpdated { .. }
            | Self::WorkspaceReloaded { .. }
            | Self::ReadinessChanged { .. } => None,
            Self::TurnTerminal { terminal, .. } => Some(*terminal),
        }
    }

    pub(crate) const fn snapshot(&self) -> &Arc<SessionExecutorSnapshot> {
        match self {
            Self::ExecutionChanged { snapshot, .. }
            | Self::DefinitionUpdated { snapshot, .. }
            | Self::MetadataUpdated { snapshot, .. }
            | Self::WorkspaceReloaded { snapshot, .. }
            | Self::ReadinessChanged { snapshot, .. }
            | Self::TurnTerminal { snapshot, .. } => snapshot,
        }
    }
}

pub(crate) struct SessionExecutorSubscription {
    snapshot: Arc<SessionExecutorSnapshot>,
    receiver: broadcast::Receiver<Arc<SessionExecutorEvent>>,
}

impl SessionExecutorSubscription {
    pub(crate) const fn snapshot(&self) -> &Arc<SessionExecutorSnapshot> {
        &self.snapshot
    }

    pub(crate) async fn recv(&mut self) -> Option<Arc<SessionExecutorEvent>> {
        self.receiver.recv().await.ok()
    }
}

impl SessionExecutionState {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// A small immutable, coherent loaded Session read model.
///
/// It intentionally exposes no live conversation, turn, model, tool, recorder, or event state.
/// The installed Session metadata is part of this immutable observation state so every executor
/// event carries the exact metadata of its moment instead of re-reading durable state later.
#[derive(Clone)]
pub(crate) struct SessionExecutorSnapshot {
    definition: Arc<SessionDefinition>,
    agent_available: bool,
    model_available: bool,
    prompt_available: bool,
    workspace_unavailable: Option<SessionUnavailableView>,
    workspace: Option<Arc<WorkspaceSnapshot>>,
    // The host security invalidation seam holds the Session in Preparing while its Workspace
    // recovery worker re-resolves the installed Workspace.  It outranks every other readiness
    // fact (Agent/workspace cause/Prompt/Model), and Preparing always projects an Idle, empty,
    // non-accepting snapshot with no WorkspaceSnapshot.
    workspace_preparing: bool,
    metadata: Arc<SessionMetadata>,
    execution_state: SessionExecutionState,
    current_turn: Option<TurnId>,
    current_turn_view: Option<CurrentTurnView>,
    active_items: Arc<[ItemView]>,
    public_pending_interactions: Arc<[InteractionView]>,
    usage: Option<SessionUsageView>,
    recording: SessionRecordingState,
    diagnostics: Arc<[SessionDiagnosticView]>,
    last_terminal: Option<(TurnId, SessionTurnTerminal)>,
    pending_interactions: Arc<[crate::live_conversation::PendingInteractionFact]>,
    active_submit_command_id: Option<CommandId>,
    follow_up_command_ids: Arc<[CommandId]>,
    steer_command_ids: Arc<[CommandId]>,
}

impl SessionExecutorSnapshot {
    fn new(
        definition: Arc<SessionDefinition>,
        agent_available: bool,
        model_available: bool,
        prompt_available: bool,
        workspace_unavailable: Option<SessionUnavailableView>,
        workspace: Option<Arc<WorkspaceSnapshot>>,
        metadata: Arc<SessionMetadata>,
        execution_state: SessionExecutionState,
    ) -> Self {
        Self {
            definition,
            agent_available,
            model_available,
            prompt_available,
            workspace_unavailable,
            workspace,
            workspace_preparing: false,
            metadata,
            execution_state,
            current_turn: None,
            current_turn_view: None,
            active_items: Arc::from([]),
            public_pending_interactions: Arc::from([]),
            usage: None,
            recording: SessionRecordingState::Healthy,
            diagnostics: Arc::from([]),
            last_terminal: None,
            pending_interactions: Arc::from([]),
            active_submit_command_id: None,
            follow_up_command_ids: Arc::from([]),
            steer_command_ids: Arc::from([]),
        }
    }

    fn with_metadata(&self, metadata: Arc<SessionMetadata>) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            metadata,
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Installs a brand-new WorkspaceSnapshot alongside a new definition and clears only the
    /// workspace Unavailable cause while installing the new definition's own model and
    /// selected-Prompt availability facts.  This is the ordinary true-Workspace definition
    /// publication and the ReloadWorkspace recovery path; it keeps every immutable observation
    /// fact and rebuilds nothing.  A disabled Agent stays AgentUnavailable until it is
    /// re-enabled, and a model or selected Prompt that cannot serve a Turn stays
    /// ModelUnavailable/PromptUnavailable until a definition publication restores it.
    ///
    /// While the host security-invalidation Preparing flag is set, the supplied
    /// WorkspaceSnapshot is deliberately not installed: a snapshot produced by a publication
    /// that settles during the invalidation is superseded by the security recovery's exact
    /// re-resolve (the recovery worker resolves the post-publication definition and installs
    /// its own snapshot at completion), so the Session keeps `workspace: None` and stays
    /// Preparing.  The new definition and its model/selected-Prompt availability facts are
    /// still installed and the workspace cause is still cleared, so the recovery always
    /// resolves the exact current definition.
    fn with_definition_and_workspace(
        &self,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        model_available: bool,
        prompt_available: bool,
    ) -> Self {
        Self {
            definition,
            agent_available: self.agent_available,
            model_available,
            prompt_available,
            workspace_unavailable: None,
            workspace: if self.workspace_preparing {
                None
            } else {
                Some(workspace)
            },
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Installs a new definition while preserving the exact current optional WorkspaceSnapshot
    /// and workspace readiness, installing the new definition's own model and selected-Prompt
    /// availability facts.  This is the future-only Model/Prompt replacement and the Agent
    /// revision upgrade; an Unavailable Session stays Unavailable with no Workspace unless the
    /// new model/Prompt facts restore it.
    fn with_definition(
        &self,
        definition: Arc<SessionDefinition>,
        model_available: bool,
        prompt_available: bool,
    ) -> Self {
        Self {
            definition,
            agent_available: self.agent_available,
            model_available,
            prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    fn with_execution(
        &self,
        execution_state: SessionExecutionState,
        current_turn: Option<TurnId>,
        last_terminal: Option<(TurnId, SessionTurnTerminal)>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state,
            current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    fn with_pending_interactions(
        &self,
        pending_interactions: Arc<[crate::live_conversation::PendingInteractionFact]>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions,
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    fn with_queue_projection(
        &self,
        active_submit_command_id: Option<CommandId>,
        follow_up_command_ids: Arc<[CommandId]>,
        steer_command_ids: Arc<[CommandId]>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id,
            follow_up_command_ids,
            steer_command_ids,
        }
    }

    fn with_public_observation(
        &self,
        current_turn_view: Option<CurrentTurnView>,
        active_items: Arc<[ItemView]>,
        public_pending_interactions: Arc<[InteractionView]>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view,
            active_items,
            public_pending_interactions,
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    fn with_public_session_state(
        &self,
        usage: Option<SessionUsageView>,
        recording: SessionRecordingState,
        diagnostics: Arc<[SessionDiagnosticView]>,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage,
            recording,
            diagnostics,
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Applies one Agent availability fact while preserving the optional WorkspaceSnapshot and
    /// every other immutable observation fact.  Enable restores the underlying workspace cause
    /// or model fact; Disable only ever produces AgentUnavailable and never drops a last-good
    /// WorkspaceSnapshot.
    fn with_agent_availability(&self, agent_available: bool) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Applies the three intended availability facts together while preserving the optional
    /// WorkspaceSnapshot and every other immutable observation fact.  This is the minimal
    /// combined update used by a Runtime shared-resource reload fan-out: the Agent fact stays
    /// the current installed value, while the precomputed model and selected-Prompt facts of the
    /// exact installed definition against the candidate resources replace the current values.
    fn with_combined_availability(
        &self,
        agent_available: bool,
        model_available: bool,
        prompt_available: bool,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available,
            model_available,
            prompt_available,
            workspace_unavailable: self.workspace_unavailable,
            workspace: self.workspace.clone(),
            workspace_preparing: self.workspace_preparing,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Enters the host security-invalidation Preparing state: the old WorkspaceSnapshot is
    /// dropped and the workspace Unavailable cause is masked (cleared) while the flag is set,
    /// so the derived public readiness is `Preparing` until the recovery final state explicitly
    /// installs the new WorkspaceSnapshot or the new Workspace/Prompt Unavailable cause.  Every
    /// other immutable observation fact (Agent/model/prompt facts, metadata, usage, recording,
    /// diagnostics, terminal, queues) is preserved.
    fn with_workspace_preparing(&self) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: None,
            workspace: None,
            workspace_preparing: true,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Finishes one successful security Workspace recovery: clears the Preparing flag, installs
    /// the exact re-resolved WorkspaceSnapshot, and clears the workspace Unavailable cause.
    fn with_workspace_preparing_finished_success(&self, workspace: Arc<WorkspaceSnapshot>) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: None,
            workspace: Some(workspace),
            workspace_preparing: false,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    /// Finishes one ordinary-failure security Workspace recovery: clears the Preparing flag,
    /// drops the WorkspaceSnapshot, and explicitly installs the new WorkspaceUnavailable or
    /// PromptUnavailable cause.
    fn with_workspace_preparing_finished_failure(
        &self,
        cause: SessionUnavailableView,
    ) -> Self {
        Self {
            definition: Arc::clone(&self.definition),
            agent_available: self.agent_available,
            model_available: self.model_available,
            prompt_available: self.prompt_available,
            workspace_unavailable: Some(cause),
            workspace: None,
            workspace_preparing: false,
            metadata: Arc::clone(&self.metadata),
            execution_state: self.execution_state,
            current_turn: self.current_turn,
            current_turn_view: self.current_turn_view,
            active_items: Arc::clone(&self.active_items),
            public_pending_interactions: Arc::clone(&self.public_pending_interactions),
            usage: self.usage.clone(),
            recording: self.recording,
            diagnostics: Arc::clone(&self.diagnostics),
            last_terminal: self.last_terminal,
            pending_interactions: Arc::clone(&self.pending_interactions),
            active_submit_command_id: self.active_submit_command_id,
            follow_up_command_ids: Arc::clone(&self.follow_up_command_ids),
            steer_command_ids: Arc::clone(&self.steer_command_ids),
        }
    }

    pub(crate) fn definition(&self) -> &Arc<SessionDefinition> {
        &self.definition
    }

    /// Ready-invariant Workspace accessor.  A Ready snapshot always carries its exact
    /// WorkspaceSnapshot; callers under the Ready invariant only use this getter, while
    /// production branches that must run for Unavailable Sessions use `workspace_optional`.
    pub(crate) fn workspace(&self) -> &Arc<WorkspaceSnapshot> {
        self.workspace
            .as_ref()
            .expect("a Ready Session snapshot always carries its WorkspaceSnapshot")
    }

    pub(crate) fn workspace_optional(&self) -> Option<&Arc<WorkspaceSnapshot>> {
        self.workspace.as_ref()
    }

    pub(crate) fn metadata(&self) -> &Arc<SessionMetadata> {
        &self.metadata
    }

    /// The installed model availability fact for the exact installed definition: whether the
    /// current Runtime-owned catalog can resolve this definition's model for a Turn.
    pub(crate) const fn model_available(&self) -> bool {
        self.model_available
    }

    /// The installed selected-Prompt availability fact for the exact installed definition:
    /// whether the current Runtime-owned Prompt resources can resolve this definition's exact
    /// Agent+Session Prompt selection for the `for_turn` selection stage.
    pub(crate) const fn prompt_available(&self) -> bool {
        self.prompt_available
    }

    /// The single public readiness projection derived from the internal facts: the host
    /// security-invalidation Preparing flag always wins, then a disabled Agent wins with
    /// AgentUnavailable, then the current workspace Unavailable cause (including a
    /// workspace-source PromptUnavailable), then an unavailable selected Prompt, then an
    /// unavailable model, then Ready.  Every mutation preserves the internal facts so this
    /// projection can never diverge from them.
    pub(crate) const fn readiness(&self) -> SessionReadinessView {
        if self.workspace_preparing {
            SessionReadinessView::Preparing
        } else if !self.agent_available {
            SessionReadinessView::Unavailable(SessionUnavailableView::AgentUnavailable)
        } else {
            match self.workspace_unavailable {
                Some(cause) => SessionReadinessView::Unavailable(cause),
                None => {
                    if !self.prompt_available {
                        SessionReadinessView::Unavailable(SessionUnavailableView::PromptUnavailable)
                    } else if !self.model_available {
                        SessionReadinessView::Unavailable(SessionUnavailableView::ModelUnavailable)
                    } else {
                        SessionReadinessView::Ready
                    }
                }
            }
        }
    }

    /// Whether the host security invalidation currently holds this Session in Preparing.
    pub(crate) const fn workspace_preparing(&self) -> bool {
        self.workspace_preparing
    }

    /// The durable definition Workspace revision, which is authoritative for both Ready and
    /// Unavailable Sessions; it avoids propagating the optional WorkspaceSnapshot.
    pub(crate) fn workspace_revision(&self) -> WorkspaceRevision {
        self.definition.workspace().revision()
    }

    pub(crate) fn definition_revision(&self) -> SessionDefinitionRevision {
        self.definition.revision()
    }

    pub(crate) const fn execution_state(&self) -> SessionExecutionState {
        self.execution_state
    }

    pub(crate) const fn current_turn(&self) -> Option<TurnId> {
        self.current_turn
    }

    pub(crate) const fn current_turn_view(&self) -> Option<CurrentTurnView> {
        self.current_turn_view
    }

    pub(crate) fn active_items(&self) -> &[ItemView] {
        &self.active_items
    }

    pub(crate) fn public_pending_interactions(&self) -> &[InteractionView] {
        &self.public_pending_interactions
    }

    pub(crate) const fn usage(&self) -> Option<&SessionUsageView> {
        self.usage.as_ref()
    }

    pub(crate) const fn recording(&self) -> SessionRecordingState {
        self.recording
    }

    pub(crate) fn diagnostics(&self) -> &[SessionDiagnosticView] {
        &self.diagnostics
    }

    pub(crate) const fn last_terminal(&self) -> Option<(TurnId, SessionTurnTerminal)> {
        self.last_terminal
    }

    pub(crate) fn pending_interactions(
        &self,
    ) -> &[crate::live_conversation::PendingInteractionFact] {
        &self.pending_interactions
    }

    pub(crate) const fn active_submit_command_id(&self) -> Option<CommandId> {
        self.active_submit_command_id
    }

    pub(crate) fn follow_up_command_ids(&self) -> &[CommandId] {
        &self.follow_up_command_ids
    }

    pub(crate) fn steer_command_ids(&self) -> &[CommandId] {
        &self.steer_command_ids
    }
}

impl fmt::Debug for SessionExecutorSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionExecutorSnapshot")
            .field("session_definition_revision", &self.definition.revision())
            .field("readiness", &self.readiness())
            .field("agent_available", &self.agent_available)
            .field("model_available", &self.model_available)
            .field("prompt_available", &self.prompt_available)
            .field(
                "workspace_revision",
                &self.workspace.as_ref().map(|workspace| workspace.revision()),
            )
            .field("workspace_preparing", &self.workspace_preparing)
            .field("metadata_revision", &self.metadata.revision())
            .field("execution_state", &self.execution_state)
            .field("current_turn", &self.current_turn)
            .field("current_turn_view", &self.current_turn_view)
            .field("active_items", &self.active_items.len())
            .field(
                "public_pending_interactions",
                &self.public_pending_interactions.len(),
            )
            .field("has_usage", &self.usage.is_some())
            .field("recording", &self.recording)
            .field("diagnostics", &self.diagnostics.len())
            .field("last_terminal", &self.last_terminal)
            .field("pending_interactions", &self.pending_interactions.len())
            .field(
                "active_submit_command_id",
                &self.active_submit_command_id.is_some(),
            )
            .field("follow_up_command_ids", &self.follow_up_command_ids.len())
            .field("steer_command_ids", &self.steer_command_ids.len())
            .finish()
    }
}

/// Resolves one Session definition's model against an installed Runtime-owned catalog through
/// the exact Turn model resolution seam.  `Ok(true)` means a Turn can be served; the three
/// ordinary model incompatibilities (selection, reasoning, output limit) mean `Ok(false)`;
/// every other resolution failure on an installed Runtime-owned catalog is an internal
/// invariant that the caller must surface through its existing fatal/internal path, never as a
/// fabricated ModelUnavailable.
pub(crate) fn model_available_for_definition(
    model_gateway: &ModelGateway,
    model_catalog: Arc<ModelCatalogView>,
    definition: &SessionDefinition,
) -> Result<bool, ModelResolutionError> {
    let model = definition.model();
    let request = ResolveTurnModelRequest::new(
        model.selection().clone(),
        model.reasoning(),
        model.max_output_tokens(),
    );
    match model_gateway.resolve_for_turn(model_catalog, request) {
        Ok(_) => Ok(true),
        Err(error) => match error.kind() {
            ModelResolutionErrorKind::ModelUnavailable
            | ModelResolutionErrorKind::UnsupportedReasoning
            | ModelResolutionErrorKind::InvalidOutputLimit => Ok(false),
            ModelResolutionErrorKind::CatalogUnavailable
            | ModelResolutionErrorKind::SourceUnavailable
            | ModelResolutionErrorKind::InvalidDefinition => Err(error),
        },
    }
}

/// The closed result of one selected-Prompt availability check for an exact Session definition
/// against the installed Runtime-owned Prompt resources.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionPromptAvailabilityError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Resolves one Session definition's selected Agent+Session Prompt selection against the
/// installed Runtime-owned Prompt resources through the exact `for_turn` selection stage.
/// `Ok(true)` means a Turn can serve this selection; the three ordinary selection failures
/// (missing Prompt, wrong role, duplicate resolved key) mean `Ok(false)`; every other failure
/// on an installed Runtime-owned Prompt view is an internal invariant that the caller must
/// surface through its existing fatal/internal path, never as a fabricated PromptUnavailable.
/// The exact retained Agent revision is read (never the current), and the returned Agent
/// identity/revision must match the definition pin exactly.
pub(crate) async fn prompt_available_for_definition(
    durable_state: DurableState,
    prompt_service: Arc<PromptService>,
    prompt_resources: Arc<PromptResourceView>,
    definition: &SessionDefinition,
) -> Result<bool, SessionPromptAvailabilityError> {
    let agent = durable_state
        .read_agent_definition(definition.agent())
        .await
        .map_err(map_agent_definition_read_to_prompt_availability)?;
    if agent.agent_id() != definition.agent().agent_id()
        || agent.revision() != definition.agent().revision()
    {
        return Err(SessionPromptAvailabilityError::InternalDispatchUnavailable);
    }
    prompt_service
        .selection_available(&prompt_resources, agent.prompts(), definition.prompts())
        .map_err(|_| SessionPromptAvailabilityError::InternalDispatchUnavailable)
}

fn map_agent_definition_read_to_prompt_availability(
    error: DurableAgentDefinitionReadError,
) -> SessionPromptAvailabilityError {
    match error {
        DurableAgentDefinitionReadError::Closing => SessionPromptAvailabilityError::Closing,
        DurableAgentDefinitionReadError::AgentNotFound
        | DurableAgentDefinitionReadError::RevisionUnavailable
        | DurableAgentDefinitionReadError::StorageUnavailable
        | DurableAgentDefinitionReadError::InternalDispatchUnavailable => {
            SessionPromptAvailabilityError::InternalDispatchUnavailable
        }
    }
}

/// The closed result of one Session definition publication CAS (ordinary definition replacement,
/// explicit Agent revision upgrade, or Workspace reload).  The name is deliberately general; the
/// old Workspace-specific names remain as crate-private type aliases for the ordinary definition
/// seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionDefinitionPublicationOutcome {
    NoChange {
        definition_revision: SessionDefinitionRevision,
        workspace_revision: WorkspaceRevision,
    },
    Updated {
        definition_revision: SessionDefinitionRevision,
        workspace_revision: WorkspaceRevision,
    },
    Reloaded {
        definition_revision: SessionDefinitionRevision,
        workspace_revision: WorkspaceRevision,
    },
}

/// Backward-compatible alias for the ordinary loaded Workspace definition seam.
pub(crate) type SessionWorkspaceDefinitionOutcome = SessionDefinitionPublicationOutcome;

impl SessionDefinitionPublicationOutcome {
    pub(crate) const fn definition_revision(self) -> SessionDefinitionRevision {
        match self {
            Self::NoChange {
                definition_revision,
                ..
            }
            | Self::Updated {
                definition_revision,
                ..
            }
            | Self::Reloaded {
                definition_revision,
                ..
            } => definition_revision,
        }
    }

    pub(crate) const fn workspace_revision(self) -> WorkspaceRevision {
        match self {
            Self::NoChange {
                workspace_revision, ..
            }
            | Self::Updated {
                workspace_revision, ..
            }
            | Self::Reloaded {
                workspace_revision, ..
            } => workspace_revision,
        }
    }

    pub(crate) const fn changed(self) -> bool {
        matches!(self, Self::Updated { .. } | Self::Reloaded { .. })
    }
}

/// Redacted failures for one loaded Session definition publication (ordinary definition
/// replacement or explicit Agent revision upgrade). The Agent-specific failures are impossible
/// on the ordinary definition path and the Workspace-specific failures are impossible on the
/// Agent upgrade path; each caller maps its impossible executor failures as internal poison.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionDefinitionPublicationError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session definition compare-and-swap is stale")]
    StaleRevision,
    #[error("Session is archived")]
    SessionArchived,
    #[error("Session is deleted")]
    SessionDeleted,
    #[error("Session upgrade targets another Agent")]
    AgentMismatch,
    #[error("Agent is disabled")]
    AgentDisabled,
    #[error("Agent is deleted")]
    AgentDeleted,
    #[error("Agent revision is unavailable")]
    RevisionUnavailable,
    #[error("durable state exceeds its selected size limit")]
    StateTooLarge,
    #[error("workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace candidate was rejected")]
    WorkspaceRejected,
    #[error("workspace authority was denied")]
    Unauthorized,
    #[error("durable storage is unavailable")]
    StorageUnavailable,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Backward-compatible alias for the ordinary loaded Workspace definition seam.
pub(crate) type SessionWorkspaceDefinitionError = SessionDefinitionPublicationError;

/// Redacted failures for the immutable snapshot request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorSnapshotError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures for the loaded Session metadata publication.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionMetadataPublishError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures for one Agent availability fact application.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionAgentAvailabilityError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures for one Runtime shared-resource installation on a loaded Session.  The
/// installation never touches DurableState; it replaces only the Prompt/Model roots inside the
/// executor's future `TurnResources` and applies the precomputed availability facts.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSharedResourceUpdateError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionInteractionError {
    #[error("session executor is closing")]
    Closing,
    #[error("interaction is not pending")]
    NotFound,
    #[error("the expected Turn does not match the Interaction owner")]
    ExpectedTurnMismatch,
    #[error("interaction resolution family does not match the pending request")]
    FamilyMismatch,
    #[error("interaction resolution is invalid for the pending request")]
    InvalidResolution,
    #[error("interaction was already resolved by another logical action")]
    AlreadyResolved,
    #[error("interaction resolution conflicts with an existing command")]
    CommandConflict,
    #[error("session interaction dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionCancelError {
    #[error("session executor is closing")]
    Closing,
    #[error("the Session is not loaded")]
    SessionNotLoaded,
    #[error("the Submit is no longer cancellable")]
    SubmitNotCancellable,
    #[error("the Turn target does not match the active Turn")]
    ExpectedTurnMismatch,
    #[error("the Turn is not running")]
    TurnNotRunning,
    #[error("the Turn is already cancelling")]
    TurnCancelling,
    #[error("the Turn is already terminal")]
    TurnTerminal,
    #[error("session cancellation dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionFollowUpError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution has no active Turn")]
    TurnNotRunning,
    #[error("the FollowUp command conflicts with an admitted command")]
    CommandConflict,
    #[error("the FollowUp queue is full")]
    QueueFull,
    #[error("session follow-up dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSteerError {
    #[error("session executor is closing")]
    Closing,
    #[error("session execution has no active Turn")]
    TurnNotRunning,
    #[error("the Turn is already cancelling")]
    TurnCancelling,
    #[error("the Steer target does not match the active Turn")]
    ExpectedTurnMismatch,
    #[error("the Steer command conflicts with an admitted command")]
    CommandConflict,
    #[error("the Steer queue is full")]
    QueueFull,
    #[error("session steer dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionQueuedMessageError {
    #[error("session executor is closing")]
    Closing,
    #[error("the queued message is not queued")]
    NotQueued,
    #[error("session queued-message dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failure from joining one loaded Session executor during Unload/shutdown.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorCloseError {
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures from preparing one loaded Session executor for graceful Unload.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorPrepareUnloadError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    Internal,
}

/// Redacted failures from executor construction.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionExecutorStartError {
    #[error("loaded Session and Workspace identities do not match")]
    SessionIdMismatch,
    #[error("loaded Session and Workspace revisions do not match")]
    WorkspaceRevisionMismatch,
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSubmitError {
    #[error("session executor is closing")]
    Closing,
    #[error("the Submit command conflicts with an in-flight command")]
    CommandConflict,
    #[error("session execution is busy")]
    SessionBusy,
    #[error("Session is not ready to accept Turns: {0:?}")]
    SessionNotReady(SessionUnavailableView),
    #[error("loaded session execution dependencies are unavailable")]
    DependencyUnavailable,
    #[error("agent is unavailable for execution")]
    AgentUnavailable,
    #[error("turn prompt capture failed")]
    Prompt,
    #[error("turn input is invalid")]
    InvalidArgument,
    #[error("turn input exceeds the model context limit")]
    ContextOverflow,
    #[error("the Submit was cancelled before Turn start")]
    Cancelled,
    #[error("Session authority was revoked before Turn start")]
    SecurityRevoked,
    #[error("the Session is preparing for unload")]
    PrepareForUnload,
    #[error("session turn dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCancelTarget {
    Submit(CommandId),
    Turn(TurnId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionCancelAccepted {
    target: SessionCancelTarget,
    cancel_epoch: u64,
}

impl SessionCancelAccepted {
    pub(crate) const fn target(self) -> SessionCancelTarget {
        self.target
    }

    pub(crate) const fn cancel_epoch(self) -> u64 {
        self.cancel_epoch
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSecurityRevokedError {
    #[error("session executor is closing")]
    Closing,
    #[error("the security-revoked target is not running")]
    NotRunning,
    #[error("the Turn target does not match the active Turn")]
    ExpectedTurnMismatch,
    #[error("the active target is already cancelling")]
    AlreadyCancelling,
    #[error("the active target is already security revoked")]
    AlreadyRevoked,
    #[error("session SecurityRevoked dispatch is unavailable")]
    InternalDispatchUnavailable,
}

/// Redacted failures of one host security Workspace invalidation.  The invalidation itself is
/// always accepted once the executor is loaded (the host has already published the hard
/// restriction fact); these errors only cover executor/registry lifecycle impossibilities.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SessionSecurityInvalidationError {
    #[error("session executor is closing")]
    Closing,
    #[error("session executor dispatch is unavailable")]
    InternalDispatchUnavailable,
}

#[derive(Clone)]
struct TurnResources {
    prompt_resources: Arc<PromptResourceView>,
    model_gateway: Arc<ModelGateway>,
    model_catalog: Arc<ModelCatalogView>,
    tool_set: Arc<ToolSet>,
    compaction: CompactionSettingsSnapshot,
}

pub(crate) struct SessionExecutorDependencies {
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    turn_resources: Option<TurnResources>,
}

impl SessionExecutorDependencies {
    pub(crate) fn with_turn_resources(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
    ) -> Self {
        Self::with_turn_resources_and_tools(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            ToolSet::empty(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one test-injected turn resource bundle binds the exact runtime owners"
    )]
    pub(crate) fn with_turn_resources_and_tools(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
    ) -> Self {
        Self::with_turn_resources_and_tools_and_compaction(
            task_context,
            durable_state,
            resolver,
            prompt_service,
            prompt_resources,
            model_gateway,
            model_catalog,
            tool_set,
            CompactionSettings::default()
                .validate()
                .expect("default compaction settings are valid"),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one Turn resource bundle binds the exact runtime owners and settings"
    )]
    pub(crate) fn with_turn_resources_and_tools_and_compaction(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        prompt_resources: Arc<PromptResourceView>,
        model_gateway: Arc<ModelGateway>,
        model_catalog: Arc<ModelCatalogView>,
        tool_set: Arc<ToolSet>,
        compaction: CompactionSettingsSnapshot,
    ) -> Self {
        Self {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources: Some(TurnResources {
                prompt_resources,
                model_gateway,
                model_catalog,
                tool_set,
                compaction,
            }),
        }
    }

    #[cfg(test)]
    fn without_turn_resources(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
    ) -> Self {
        Self {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources: None,
        }
    }
}

struct TurnAdmissionGate {
    open: Mutex<bool>,
}

impl TurnAdmissionGate {
    fn new() -> Self {
        Self {
            open: Mutex::new(true),
        }
    }

    fn close(&self) {
        *lock(&self.open) = false;
    }

    /// Reopens the admission gate after a security Workspace recovery finishes.  Unavailable
    /// Sessions are still rejected early by the readiness check, so the gate can be open while
    /// the Session is not Ready.
    fn open(&self) {
        *lock(&self.open) = true;
    }

    fn try_enter(&self) -> Option<TurnAdmissionPermit<'_>> {
        let guard = lock(&self.open);
        if !*guard {
            return None;
        }
        Some(TurnAdmissionPermit { _guard: guard })
    }
}

struct TurnAdmissionPermit<'a> {
    _guard: MutexGuard<'a, bool>,
}

/// A typed process-local exclusion for one definition publication.
///
/// The identity is intentionally opaque.  Only the actor and the owner-retained completion can
/// compare it, so a completion from a different publication cannot install a snapshot.
#[derive(Clone)]
pub(crate) struct SessionDefinitionPublicationPermit {
    identity: Arc<PublicationPermitIdentity>,
}

struct PublicationPermitIdentity;

impl SessionDefinitionPublicationPermit {
    fn new() -> Self {
        Self {
            identity: Arc::new(PublicationPermitIdentity),
        }
    }

    fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl fmt::Debug for SessionDefinitionPublicationPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionDefinitionPublicationPermit { .. }")
    }
}

#[derive(Clone)]
pub(crate) struct LoadedSessionConversation {
    live_state: Arc<Mutex<LiveSessionState>>,
    recorder: Arc<SessionRecorder>,
    replay_diagnostics: ConversationReplayDiagnostics,
    active_control_generation: ActiveControlGeneration,
}

/// Process-local identity for the control owner of one active Turn.  The worker keeps the exact
/// identity it was admitted with and compares it against the actor-owned current identity before
/// every logical retry.
struct ControlGeneration(u8);

type ActiveControlGeneration = Arc<Mutex<Option<(TurnId, Arc<ControlGeneration>)>>>;

impl LoadedSessionConversation {
    pub(crate) fn from_replay(
        live_state: LiveSessionState,
        recorder: SessionRecorder,
        replay_diagnostics: ConversationReplayDiagnostics,
    ) -> Self {
        Self {
            live_state: Arc::new(Mutex::new(live_state)),
            recorder: Arc::new(recorder),
            replay_diagnostics,
            active_control_generation: Arc::new(Mutex::new(None)),
        }
    }

    fn install_control_generation(&self, turn_id: TurnId, generation: Arc<ControlGeneration>) {
        *lock(&self.active_control_generation) = Some((turn_id, generation));
    }

    fn clear_control_generation(&self, turn_id: TurnId, generation: &Arc<ControlGeneration>) {
        let mut current = lock(&self.active_control_generation);
        if current
            .as_ref()
            .is_some_and(|(current_turn, current_generation)| {
                *current_turn == turn_id && Arc::ptr_eq(current_generation, generation)
            })
        {
            *current = None;
        }
    }

    fn has_control_generation(&self, turn_id: TurnId, generation: &Arc<ControlGeneration>) -> bool {
        lock(&self.active_control_generation).as_ref().is_some_and(
            |(current_turn, current_generation)| {
                *current_turn == turn_id && Arc::ptr_eq(current_generation, generation)
            },
        )
    }

    #[cfg(test)]
    fn invalidate_control_generation_for_test(&self, turn_id: TurnId) {
        let mut current = lock(&self.active_control_generation);
        if current
            .as_ref()
            .is_some_and(|(current_turn, _)| *current_turn == turn_id)
        {
            *current = None;
        }
    }
}

/// The loaded Session control actor handle.
#[derive(Clone)]
pub(crate) struct SessionExecutor {
    sender: mpsc::Sender<SessionExecutorRequest>,
    emergency_sender: mpsc::UnboundedSender<SessionExecutorRequest>,
    closing: CancellationToken,
    task: TrackedTask,
    failure_state: Arc<ActorFailureState>,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    published_snapshot: Arc<Mutex<Arc<SessionExecutorSnapshot>>>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    events: broadcast::Sender<Arc<SessionExecutorEvent>>,
    emergency: EmergencyControlHandle,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

impl fmt::Debug for SessionExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionExecutor { .. }")
    }
}

impl SessionExecutor {
    pub(crate) fn capture_fork_conversation(
        &self,
        anchor: ForkAnchor,
    ) -> Result<CapturedForkConversation, ForkAnchorResolutionError> {
        let conversation = self
            .conversation
            .as_ref()
            .ok_or(ForkAnchorResolutionError::InvalidSource)?;
        lock(&conversation.live_state).capture_fork_conversation(anchor)
    }

    /// Starts a test-only loaded Ready+Idle Session without Turn resources.
    #[cfg(test)]
    pub(crate) fn start_loaded_ready_idle(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_idle_inner(
            SessionExecutorDependencies::without_turn_resources(
                task_context,
                durable_state,
                resolver,
                prompt_service,
            ),
            definition,
            true,
            true,
            true,
            None,
            Some(workspace),
            Some(Arc::new(conversation)),
            CancellationToken::new(),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_loaded_ready_idle_with_turn_resources(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_ready_idle_with_turn_resources_and_lifecycle(
            dependencies,
            definition,
            workspace,
            conversation,
            CancellationToken::new(),
        )
    }

    pub(crate) fn start_loaded_ready_idle_with_turn_resources_and_lifecycle(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
        conversation: LoadedSessionConversation,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_idle_inner(
            dependencies,
            definition,
            true,
            true,
            true,
            None,
            Some(workspace),
            Some(Arc::new(conversation)),
            lifecycle_closing,
        )
    }

    /// Starts a loaded Idle Session that is not Ready for a workspace cause: it carries no
    /// WorkspaceSnapshot and settles every Submit with the given Unavailable cause until a
    /// Workspace reload or a true Workspace definition publication restores Ready.  The Agent,
    /// model and selected-Prompt availability facts default to enabled/available.
    pub(crate) fn start_loaded_unavailable_idle_with_turn_resources_and_lifecycle(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        unavailable: SessionUnavailableView,
        conversation: LoadedSessionConversation,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_idle_inner(
            dependencies,
            definition,
            true,
            true,
            true,
            Some(unavailable),
            None,
            Some(Arc::new(conversation)),
            lifecycle_closing,
        )
    }

    /// The production Load seam that combines the four independent availability facts: the
    /// Agent availability fact, the model availability fact and the selected-Prompt
    /// availability fact for the captured definition, and the optional captured
    /// WorkspaceSnapshot with the current workspace Unavailable cause, so a disabled/deleted
    /// Agent, an unresolvable model or selected Prompt, or an unavailable Workspace still
    /// loads its last-good Workspace while projecting its own Unavailable cause until that
    /// fact changes.
    #[allow(
        clippy::too_many_arguments,
        reason = "the production Load seam atomically installs the four availability facts, the optional Workspace, the conversation, and the lifecycle closing token"
    )]
    pub(crate) fn start_loaded_idle_with_turn_resources_and_lifecycle(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        agent_available: bool,
        model_available: bool,
        prompt_available: bool,
        workspace_unavailable: Option<SessionUnavailableView>,
        workspace: Option<Arc<WorkspaceSnapshot>>,
        conversation: LoadedSessionConversation,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_idle_inner(
            dependencies,
            definition,
            agent_available,
            model_available,
            prompt_available,
            workspace_unavailable,
            workspace,
            Some(Arc::new(conversation)),
            lifecycle_closing,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_loaded_ready_idle_without_conversation(
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        resolver: Arc<WorkspaceResolver>,
        prompt_service: Arc<PromptService>,
        definition: Arc<SessionDefinition>,
        workspace: Arc<WorkspaceSnapshot>,
    ) -> Result<Self, SessionExecutorStartError> {
        Self::start_loaded_idle_inner(
            SessionExecutorDependencies::without_turn_resources(
                task_context,
                durable_state,
                resolver,
                prompt_service,
            ),
            definition,
            true,
            true,
            true,
            None,
            Some(workspace),
            None,
            CancellationToken::new(),
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic Load installs the four availability facts, the optional Workspace, the conversation, and the lifecycle closing token"
    )]
    fn start_loaded_idle_inner(
        dependencies: SessionExecutorDependencies,
        definition: Arc<SessionDefinition>,
        agent_available: bool,
        model_available: bool,
        prompt_available: bool,
        workspace_unavailable: Option<SessionUnavailableView>,
        workspace: Option<Arc<WorkspaceSnapshot>>,
        conversation: Option<Arc<LoadedSessionConversation>>,
        lifecycle_closing: CancellationToken,
    ) -> Result<Self, SessionExecutorStartError> {
        let SessionExecutorDependencies {
            task_context,
            durable_state,
            resolver,
            prompt_service,
            turn_resources,
        } = dependencies;
        // Session identity and Workspace revision agreement only exist for a captured
        // WorkspaceSnapshot; an Unavailable Session deliberately has none.
        if let Some(workspace) = workspace.as_ref() {
            if definition.session_id() != workspace.session_id() {
                return Err(SessionExecutorStartError::SessionIdMismatch);
            }
            if definition.workspace().revision() != workspace.revision() {
                return Err(SessionExecutorStartError::WorkspaceRevisionMismatch);
            }
        }
        // The Load operation installs the durable current metadata into the immutable observation
        // state synchronously under the per-Session gate, so every later executor event carries the
        // exact metadata of its moment instead of re-reading durable state at subscription time.
        // The durable head must still agree with the loaded identity, lifecycle, and definition;
        // any disagreement means the gate-coherent snapshot this executor was built from is stale.
        let current = match durable_state.session_current(definition.session_id()) {
            Some(current)
                if current.head().session_id() == definition.session_id()
                    && current.head().lifecycle() == SessionLifecycle::Open
                    && current.definition() == &definition =>
            {
                current
            }
            _ => return Err(SessionExecutorStartError::InternalDispatchUnavailable),
        };
        let metadata = Arc::new(current.head().metadata().clone());

        let (usage, recording, diagnostics) = capture_public_session_state(conversation.as_ref());
        let current = Arc::new(
            SessionExecutorSnapshot::new(
                Arc::clone(&definition),
                agent_available,
                model_available,
                prompt_available,
                workspace_unavailable,
                workspace,
                metadata,
                SessionExecutionState::Idle,
            )
            .with_public_session_state(usage, recording, diagnostics),
        );
        let published_snapshot = Arc::new(Mutex::new(Arc::clone(&current)));
        let (sender, receiver) = mpsc::channel(SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY);
        let (emergency_sender, emergency_receiver) = mpsc::unbounded_channel();
        let (completion_sender, completion_receiver) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(SESSION_EVENT_CAPACITY);
        let closing = CancellationToken::new();
        let turn_admission_gate = Arc::new(TurnAdmissionGate::new());
        let emergency = EmergencyControlHandle::new();
        #[cfg(test)]
        let hooks = Arc::new(SessionExecutorTestHooksInner::new());
        let failure_state = Arc::new(ActorFailureState::default());
        let actor = SessionExecutorActor {
            receiver,
            emergency_receiver,
            completions: completion_receiver,
            completion_sender,
            closing: closing.clone(),
            lifecycle_closing,
            task_context: task_context.clone(),
            durable_state: durable_state.clone(),
            resolver,
            prompt_service,
            current,
            published_snapshot: Arc::clone(&published_snapshot),
            execution_state: SessionExecutionState::Idle,
            active_publication: None,
            failure_state: Arc::clone(&failure_state),
            conversation: conversation.clone(),
            turn_resources,
            active_admission: None,
            active_turn: None,
            pending_interactions: BTreeMap::new(),
            follow_up: FollowUpQueue::new(),
            steer: SteerQueue::new(),
            turn_admission_gate: Arc::clone(&turn_admission_gate),
            events: events.clone(),
            emergency: emergency.clone(),
            pending_availability: None,
            prepare_unload: None,
            prepare_unload_accepted: false,
            security_invalidation: None,
            #[cfg(test)]
            hooks: Arc::clone(&hooks),
        };
        let mut exit_guard = ActorExitGuard::new(
            closing.clone(),
            task_context.clone(),
            durable_state.clone(),
            Arc::clone(&failure_state),
            Arc::clone(&turn_admission_gate),
        );
        let task = match task_context.spawn_tracked(async move {
            let normal_exit = actor.run().await;
            if normal_exit {
                exit_guard.disarm();
            }
        }) {
            Ok(task) => task,
            Err(RuntimeTaskError::OwnerClosing) => {
                // The guard has no admitted waiter here, but it still closes the durable owner as
                // required for an actor that could not be installed.
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionExecutorStartError::Closing);
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                task_context.request_closing();
                durable_state.request_closing();
                return Err(SessionExecutorStartError::InternalDispatchUnavailable);
            }
        };

        Ok(Self {
            sender,
            emergency_sender,
            closing,
            task,
            failure_state,
            turn_admission_gate,
            published_snapshot,
            conversation,
            events,
            emergency,
            #[cfg(test)]
            hooks,
        })
    }

    /// Requests the actor to reject future requests.  An admitted publication may abandon
    /// cancellable candidate capture, but work that has reached durable publication still drains.
    pub(crate) fn request_closing(&self) {
        self.turn_admission_gate.close();
        self.closing.cancel();
    }

    /// Closes this executor, drains accepted requests, waits the admitted publication, and waits
    /// for the owner-tracked actor settlement.  It never shuts down the shared RuntimeTaskContext.
    pub(crate) async fn close(&self) -> Result<(), SessionExecutorCloseError> {
        self.request_closing();
        let task_result = self.task.wait().await;
        if let Some(conversation) = &self.conversation {
            conversation.recorder.close().await;
        }
        if task_result.is_err() || self.failure_state.is_fatal() {
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        } else {
            Ok(())
        }
    }

    /// Starts one graceful-Unload preparation.  The admission gate is closed synchronously, then
    /// the deadline and its waiter are handed to the actor over the unbounded emergency lane so
    /// this request can never be blocked behind the bounded work lane.  The returned waiter
    /// resolves once the actor has no active publication, admission, or Turn; an earlier
    /// duplicate request keeps the effective deadline (a later deadline never extends it), and
    /// once the deadline has fired a duplicate only joins the waiters without re-triggering it.
    pub(crate) fn begin_prepare_for_unload(
        &self,
        grace: std::time::Duration,
    ) -> Result<PrepareUnloadWaiter, SessionExecutorPrepareUnloadError> {
        // The gate is the synchronous admission stop: no in-flight or future admission can pass
        // it, so the actor only has to drain work that already entered.
        self.turn_admission_gate.close();
        let (response, receiver) = oneshot::channel();
        let request = SessionExecutorRequest::PrepareUnload(PrepareUnloadRequest {
            deadline: tokio::time::Instant::now()
                .checked_add(grace)
                .unwrap_or_else(tokio::time::Instant::now),
            response: Some(response),
        });
        if let Err(error) = self.emergency_sender.send(request) {
            // The actor is gone; the dropped request settles its waiter with Closing.
            drop(error.0);
            return Err(SessionExecutorPrepareUnloadError::Closing);
        }
        Ok(PrepareUnloadWaiter {
            receiver,
            failure_state: Arc::clone(&self.failure_state),
        })
    }

    /// Prepares this executor for graceful Unload: closes admission, lets the grace period run,
    /// and returns once the actor is Idle (no active publication, admission, or Turn).
    pub(crate) async fn prepare_for_unload(
        &self,
        grace: std::time::Duration,
    ) -> Result<(), SessionExecutorPrepareUnloadError> {
        let waiter = self.begin_prepare_for_unload(grace)?;
        waiter.wait().await
    }

    /// Returns the last coherent immutable loaded snapshot.  Requests sent while a publication
    /// is in flight observe the old snapshot until the actor installs the new one.
    pub(crate) async fn snapshot(
        &self,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Snapshot(SnapshotRequest {
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionExecutorSnapshotError::Closing)
            } else {
                Err(SessionExecutorSnapshotError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) fn published_snapshot(&self) -> Arc<SessionExecutorSnapshot> {
        Arc::clone(&lock(&self.published_snapshot))
    }

    /// Publishes a complete lowered Workspace replacement through the loaded Session actor.
    pub(crate) async fn update_workspace_definition(
        &self,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError> {
        self.update_workspace_definition_with_cancellation(
            expected_revision,
            workspace,
            owner_timestamp,
            CancellationToken::new(),
        )
        .await
    }

    pub(crate) async fn update_workspace_definition_with_cancellation(
        &self,
        expected_revision: SessionDefinitionRevision,
        workspace: Workspace,
        owner_timestamp: Timestamp,
        candidate_cancellation: CancellationToken,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError> {
        self.update_session_definition_with_cancellation(
            expected_revision,
            Some(workspace),
            None,
            None,
            owner_timestamp,
            CommandId::generate().expect("test wrapper generates a process-local command id"),
            candidate_cancellation,
        )
        .await
    }

    /// Publishes one complete lowered Session definition replacement through the loaded Session
    /// actor.  The actor decides whether the candidate changes Workspace semantics (by comparing
    /// the owner-materialized candidate WorkspaceRevision with the installed revision) and only
    /// then applies the loaded Idle requirement or the prebuilt Workspace Snapshot installation.
    #[allow(
        clippy::too_many_arguments,
        reason = "one atomic definition publication carries its CAS token, three replacements, owner event facts, and cancellation"
    )]
    pub(crate) async fn update_session_definition_with_cancellation(
        &self,
        expected_revision: SessionDefinitionRevision,
        workspace: Option<Workspace>,
        model: Option<SessionModelConfig>,
        prompts: Option<SessionPromptSelection>,
        owner_timestamp: Timestamp,
        command_id: CommandId,
        candidate_cancellation: CancellationToken,
    ) -> Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Update(WorkspaceDefinitionRequest {
            expected_revision,
            workspace,
            model,
            prompts,
            owner_timestamp,
            command_id,
            candidate_cancellation,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable));
                }
            },
        };
        // Reserving the bounded sender is the admission point.  No cancellable await occurs
        // between it and handing ownership of the request to the actor.
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionWorkspaceDefinitionError::Closing)
            } else {
                Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
            }
        })
    }

    /// Publishes one explicit Session Agent revision upgrade through the loaded Session actor.
    /// The executor never resolves target current, retained membership, Agent status, or a
    /// candidate definition; only DurableState resolves those facts under its Agent → Session
    /// publication gates.  The actor prechecks only the installed expected revision, publication
    /// busy, and closing/cancellation, then validates the exact durable outcome before install.
    pub(crate) async fn upgrade_session_agent_with_cancellation(
        &self,
        expected_revision: SessionDefinitionRevision,
        target: Option<AgentRevisionRef>,
        owner_timestamp: Timestamp,
        command_id: CommandId,
        candidate_cancellation: CancellationToken,
    ) -> Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::UpgradeAgent(AgentUpgradeRequest {
            expected_revision,
            target,
            owner_timestamp,
            command_id,
            candidate_cancellation,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionDefinitionPublicationError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(
                        SessionDefinitionPublicationError::InternalDispatchUnavailable,
                    ));
                }
            },
        };
        // Reserving the bounded sender is the admission point.  No cancellable await occurs
        // between it and handing ownership of the request to the actor.
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionDefinitionPublicationError::Closing)
            } else {
                Err(SessionDefinitionPublicationError::InternalDispatchUnavailable)
            }
        })
    }

    /// Reloads the loaded Session's installed Workspace through the actor's single active
    /// publication slot.  The worker re-resolves the exact currently installed definition
    /// Workspace, captures Workspace Prompt sources, revalidates the required authority, and
    /// finishes one exact WorkspaceSnapshot; it never calls DurableState.  The actor validates
    /// the exact admission definition and Snapshot shape before atomically replacing only the
    /// WorkspaceSnapshot Arc, preserving the exact definition Arc and every other loaded fact.
    pub(crate) async fn reload_workspace_with_cancellation(
        &self,
        owner_timestamp: Timestamp,
        command_id: CommandId,
        candidate_cancellation: CancellationToken,
    ) -> Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::ReloadWorkspace(ReloadWorkspaceRequest {
            owner_timestamp,
            command_id,
            candidate_cancellation,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionDefinitionPublicationError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(
                        SessionDefinitionPublicationError::InternalDispatchUnavailable,
                    ));
                }
            },
        };
        // Reserving the bounded sender is the admission point.  No cancellable await occurs
        // between it and handing ownership of the request to the actor.
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionDefinitionPublicationError::Closing)
            } else {
                Err(SessionDefinitionPublicationError::InternalDispatchUnavailable)
            }
        })
    }

    /// Applies one Agent availability fact to the loaded Session without touching DurableState.
    /// The actor immediately updates an Idle Session that has no active admission/Turn and only
    /// publishes `ReadinessChanged` when the public readiness actually changes; during
    /// Starting/Running/Finishing (or an active admission/Turn) it saves the latest pending fact
    /// and applies it when the Session returns to Idle.
    pub(crate) async fn set_agent_availability_with_cancellation(
        &self,
        agent_id: crate::wire::AgentId,
        available: bool,
        timestamp: Timestamp,
        command_id: CommandId,
        candidate_cancellation: CancellationToken,
    ) -> Result<(), SessionAgentAvailabilityError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::SetAgentAvailability(AgentAvailabilityRequest {
            agent_id,
            available,
            timestamp,
            command_id,
            candidate_cancellation,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionAgentAvailabilityError::Closing));
            }
            _ = candidate_cancellation.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionAgentAvailabilityError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionAgentAvailabilityError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionAgentAvailabilityError::Closing)
            } else {
                Err(SessionAgentAvailabilityError::InternalDispatchUnavailable)
            }
        })
    }

    /// Installs one Runtime shared-resource pair on the loaded Session without touching
    /// DurableState.  The actor validates by `Arc::ptr_eq` that the exact expected definition is
    /// still installed, replaces only the Prompt/Model roots of the future `turn_resources`, and
    /// applies the precomputed facts: immediately for an Idle Session with no active
    /// admission/Turn (publishing `ReadinessChanged` only when the public readiness actually
    /// changes), or merged into the latest intended availability composite for any other
    /// Session, applied when it returns to Idle.
    #[allow(
        clippy::too_many_arguments,
        reason = "one shared-resource installation carries its exact definition identity, the new Prompt/Model roots, the two precomputed facts, and owner event facts"
    )]
    pub(crate) async fn update_shared_resources_with_cancellation(
        &self,
        expected_definition: Arc<SessionDefinition>,
        prompt_resources: Arc<PromptResourceView>,
        model_catalog: Arc<ModelCatalogView>,
        prompt_available: bool,
        model_available: bool,
        timestamp: Timestamp,
        command_id: CommandId,
        candidate_cancellation: CancellationToken,
    ) -> Result<(), SessionSharedResourceUpdateError> {
        let (response, waiter) = oneshot::channel();
        let mut request =
            SessionExecutorRequest::UpdateSharedResources(SharedResourceUpdateRequest {
                expected_definition,
                prompt_resources,
                model_catalog,
                prompt_available,
                model_available,
                timestamp,
                command_id,
                candidate_cancellation,
                response: Some(response),
            });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSharedResourceUpdateError::Closing));
            }
            _ = candidate_cancellation.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSharedResourceUpdateError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionSharedResourceUpdateError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionSharedResourceUpdateError::Closing)
            } else {
                Err(SessionSharedResourceUpdateError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn publish_metadata(
        &self,
        metadata: Arc<SessionMetadata>,
        timestamp: Timestamp,
        command_id: CommandId,
    ) -> Result<Arc<SessionExecutorSnapshot>, SessionMetadataPublishError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::PublishMetadata(UpdateSessionMetadataRequest {
            metadata,
            timestamp,
            command_id,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionMetadataPublishError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionMetadataPublishError::InternalDispatchUnavailable));
                }
            },
        };
        // Reserving the bounded sender is the admission point.  No cancellable await occurs
        // between it and handing ownership of the request to the actor.
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionMetadataPublishError::Closing)
            } else {
                Err(SessionMetadataPublishError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn submit(
        &self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<TurnId, SessionSubmitError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Submit(SubmitRequest {
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSubmitError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionSubmitError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionSubmitError::Closing)
            } else {
                Err(SessionSubmitError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn resolve_interaction(
        &self,
        expected_turn_id: TurnId,
        item_id: ItemId,
        request_id: RequestId,
        resolution_key: InteractionResolutionKey,
        resolution: InteractionResolutionInput,
        timestamp: Timestamp,
    ) -> Result<(), SessionInteractionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::ResolveInteraction(ResolveInteractionRequest {
            expected_turn_id,
            item_id,
            request_id,
            resolution_key,
            resolution: Some(resolution),
            timestamp,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionInteractionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionInteractionError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionInteractionError::Closing)
            } else {
                Err(SessionInteractionError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn cancel(
        &self,
        target: SessionCancelTarget,
        timestamp: Timestamp,
    ) -> Result<SessionCancelAccepted, SessionCancelError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Cancel(CancelRequest {
            target,
            timestamp,
            response: Some(response),
        });
        if self.closing.is_cancelled() {
            request.reject_closing();
            return waiter.await.unwrap_or(Err(SessionCancelError::Closing));
        }
        if let Err(error) = self.emergency_sender.send(request) {
            let mut request = error.0;
            request.reject_closing();
            return waiter
                .await
                .unwrap_or(Err(SessionCancelError::InternalDispatchUnavailable));
        }
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled()
                || self.sender.is_closed()
                || self.emergency_sender.is_closed()
            {
                Err(SessionCancelError::Closing)
            } else {
                Err(SessionCancelError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn security_revoke(
        &self,
        target: SessionCancelTarget,
    ) -> Result<(), SessionSecurityRevokedError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::SecurityRevoked(SecurityRevokedRequest {
            target,
            response: Some(response),
        });
        if self.closing.is_cancelled() {
            request.reject_closing();
            return waiter
                .await
                .unwrap_or(Err(SessionSecurityRevokedError::Closing));
        }
        if let Err(error) = self.emergency_sender.send(request) {
            let mut request = error.0;
            request.reject_closing();
            return waiter.await.unwrap_or(Err(
                SessionSecurityRevokedError::InternalDispatchUnavailable,
            ));
        }
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled()
                || self.sender.is_closed()
                || self.emergency_sender.is_closed()
            {
                Err(SessionSecurityRevokedError::Closing)
            } else {
                Err(SessionSecurityRevokedError::InternalDispatchUnavailable)
            }
        })
    }

    /// Starts one host security Workspace invalidation.  The admission gate is closed
    /// synchronously, then the request is handed to the actor over the unbounded emergency lane
    /// so it can never be blocked behind the bounded work lane.  The returned waiter resolves
    /// once the actor has installed the recovery final state (the re-resolved WorkspaceSnapshot
    /// or the new Workspace/Prompt Unavailable cause); a duplicate invalidation joins the same
    /// recovery without re-signaling or re-running it.  A dropped waiter never cancels the
    /// admitted recovery, and an old handle can never forward its signal to a future
    /// replacement executor: the request is owned by this exact actor from the send point.  A
    /// send failure means the actor is already gone: it reports Closing, or
    /// InternalDispatchUnavailable when the executor failed fatally (a fatal executor must not
    /// be downgraded to an ordinary Closing by the residency mapping).
    pub(crate) fn begin_security_invalidation(
        &self,
        timestamp: Timestamp,
    ) -> Result<SecurityInvalidationWaiter, SessionSecurityInvalidationError> {
        // The gate is the synchronous admission stop: no in-flight or future admission can pass
        // it, so the actor only has to drain work that already entered.
        self.turn_admission_gate.close();
        let (response, receiver) = oneshot::channel();
        let request = SessionExecutorRequest::SecurityInvalidation(SecurityInvalidationRequest {
            timestamp,
            response: Some(response),
        });
        if let Err(error) = self.emergency_sender.send(request) {
            // The actor is gone and nobody will poll the receiver, so the failure_state is the
            // truth here: a fatal executor reports Internal, an ordinary close reports Closing.
            drop(error.0);
            if self.failure_state.is_fatal() {
                return Err(SessionSecurityInvalidationError::InternalDispatchUnavailable);
            }
            return Err(SessionSecurityInvalidationError::Closing);
        }
        Ok(SecurityInvalidationWaiter {
            receiver,
            failure_state: Arc::clone(&self.failure_state),
        })
    }

    /// Queues a FollowUp behind the active Turn.  Public Runtime routing is layered above this
    /// owner-local seam; snapshot queue projection remains a separate read-model slice.
    pub(crate) async fn follow_up(
        &self,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionFollowUpError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::FollowUp(FollowUpRequest {
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionFollowUpError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionFollowUpError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionFollowUpError::Closing)
            } else {
                Err(SessionFollowUpError::InternalDispatchUnavailable)
            }
        })
    }

    /// Queues a Steer for the active Turn.  Public Runtime routing is layered above this
    /// owner-local seam; consumption is performed by the active Turn worker at a complete
    /// assistant/tool safe point.
    pub(crate) async fn steer(
        &self,
        turn_id: TurnId,
        command_id: CommandId,
        intent: PromptIntent,
    ) -> Result<(), SessionSteerError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Steer(SteerRequest {
            turn_id,
            command_id,
            intent: Some(intent),
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionSteerError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionSteerError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionSteerError::Closing)
            } else {
                Err(SessionSteerError::InternalDispatchUnavailable)
            }
        })
    }

    /// Removes one admitted Steer or FollowUp by CommandId.  Public Runtime routing is layered
    /// above this owner-local seam; snapshot queue projection remains a separate read-model
    /// slice.
    pub(crate) async fn cancel_queued_message(
        &self,
        command_id: CommandId,
    ) -> Result<(), SessionQueuedMessageError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::CancelQueuedMessage(CancelQueuedMessageRequest {
            command_id,
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionQueuedMessageError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionQueuedMessageError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionQueuedMessageError::Closing)
            } else {
                Err(SessionQueuedMessageError::InternalDispatchUnavailable)
            }
        })
    }

    pub(crate) async fn subscribe(
        &self,
    ) -> Result<SessionExecutorSubscription, SessionExecutorSnapshotError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::Subscribe(SubscribeRequest {
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionExecutorSnapshotError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or(Err(
            SessionExecutorSnapshotError::InternalDispatchUnavailable,
        ))
    }

    #[cfg(test)]
    pub(crate) fn test_hooks(&self) -> SessionExecutorTestHooks {
        SessionExecutorTestHooks {
            inner: Arc::clone(&self.hooks),
        }
    }

    #[cfg(test)]
    pub(crate) fn emergency_control_for_test(&self) -> EmergencyControlHandle {
        self.emergency.clone()
    }

    #[cfg(test)]
    pub(crate) async fn wait_until_closing_for_test(&self) {
        self.closing.cancelled().await;
    }

    #[cfg(test)]
    pub(crate) fn live_state_for_test(&self) -> Option<Arc<Mutex<LiveSessionState>>> {
        self.conversation
            .as_ref()
            .map(|conversation| Arc::clone(&conversation.live_state))
    }

    #[cfg(test)]
    pub(crate) fn invalidate_control_generation_for_test(&self, turn_id: TurnId) {
        if let Some(conversation) = &self.conversation {
            conversation.invalidate_control_generation_for_test(turn_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn retry_basis_matches_for_test(
        &self,
        turn_id: TurnId,
        source_revision: crate::live_conversation::ConversationRevision,
    ) -> Option<bool> {
        let conversation = self.conversation.as_ref()?;
        let generation = lock(&conversation.active_control_generation)
            .as_ref()
            .and_then(|(current_turn, generation)| {
                (*current_turn == turn_id).then(|| Arc::clone(generation))
            })?;
        let emergency = self
            .emergency
            .observe(EmergencyControlTarget::Turn(turn_id))?;
        Some(retry_basis_is_current(
            conversation,
            turn_id,
            &generation,
            source_revision,
            &self.emergency,
            emergency,
        ))
    }

    #[cfg(test)]
    pub(crate) fn recorder_for_test(&self) -> Option<Arc<SessionRecorder>> {
        self.conversation
            .as_ref()
            .map(|conversation| Arc::clone(&conversation.recorder))
    }

    #[cfg(test)]
    pub(crate) fn replay_diagnostics_for_test(&self) -> Option<ConversationReplayDiagnostics> {
        self.conversation
            .as_ref()
            .map(|conversation| conversation.replay_diagnostics.clone())
    }

    #[cfg(test)]
    pub(crate) async fn starting_admission_probe_for_test(
        &self,
    ) -> Result<(), SessionWorkspaceDefinitionError> {
        let (response, waiter) = oneshot::channel();
        let mut request = SessionExecutorRequest::StartingProbe(StartingProbeRequest {
            response: Some(response),
        });
        let permit = tokio::select! {
            biased;
            _ = self.closing.cancelled() => {
                request.reject_closing();
                return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::Closing));
            }
            permit = self.sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => {
                    request.reject_closing();
                    return waiter.await.unwrap_or(Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable));
                }
            },
        };
        permit.send(request);
        waiter.await.unwrap_or_else(|_| {
            if self.closing.is_cancelled() || self.sender.is_closed() {
                Err(SessionWorkspaceDefinitionError::Closing)
            } else {
                Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
            }
        })
    }
}

/// The actor-owned shared settlement waiter of one graceful-Unload preparation.  A duplicate
/// `PrepareUnload` request joins the same actor state, so every caller waits on the same Idle
/// settlement and the effective deadline can only shorten until it fires.
pub(crate) struct PrepareUnloadWaiter {
    receiver: oneshot::Receiver<Result<(), SessionExecutorPrepareUnloadError>>,
    failure_state: Arc<ActorFailureState>,
}

impl PrepareUnloadWaiter {
    pub(crate) async fn wait(self) -> Result<(), SessionExecutorPrepareUnloadError> {
        self.receiver.await.unwrap_or_else(|_| {
            if self.failure_state.is_fatal() {
                Err(SessionExecutorPrepareUnloadError::Internal)
            } else {
                Err(SessionExecutorPrepareUnloadError::Closing)
            }
        })
    }
}

/// The host security-invalidation waiter.  It resolves once the actor has installed the
/// recovery final state (the re-resolved WorkspaceSnapshot or the new Workspace/Prompt
/// Unavailable cause), or settles Closing/Internal when the executor closes or fails first.
/// A dropped host waiter never cancels the admitted recovery: the request is owned by the actor
/// from the moment it is sent over the emergency lane.
pub(crate) struct SecurityInvalidationWaiter {
    receiver: oneshot::Receiver<Result<(), SessionSecurityInvalidationError>>,
    failure_state: Arc<ActorFailureState>,
}

impl SecurityInvalidationWaiter {
    pub(crate) async fn wait(self) -> Result<(), SessionSecurityInvalidationError> {
        self.receiver.await.unwrap_or_else(|_| {
            if self.failure_state.is_fatal() {
                Err(SessionSecurityInvalidationError::InternalDispatchUnavailable)
            } else {
                Err(SessionSecurityInvalidationError::Closing)
            }
        })
    }
}

struct SessionExecutorActor {
    receiver: mpsc::Receiver<SessionExecutorRequest>,
    emergency_receiver: mpsc::UnboundedReceiver<SessionExecutorRequest>,
    completions: mpsc::UnboundedReceiver<ExecutorCompletion>,
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    closing: CancellationToken,
    lifecycle_closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    current: Arc<SessionExecutorSnapshot>,
    published_snapshot: Arc<Mutex<Arc<SessionExecutorSnapshot>>>,
    execution_state: SessionExecutionState,
    active_publication: Option<ActivePublication>,
    failure_state: Arc<ActorFailureState>,
    conversation: Option<Arc<LoadedSessionConversation>>,
    turn_resources: Option<TurnResources>,
    active_admission: Option<ActiveAdmission>,
    active_turn: Option<ActiveTurn>,
    pending_interactions: BTreeMap<RequestId, ActiveInteraction>,
    follow_up: FollowUpQueue,
    steer: SteerQueue,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    events: broadcast::Sender<Arc<SessionExecutorEvent>>,
    emergency: EmergencyControlHandle,
    pending_availability: Option<PendingAvailability>,
    prepare_unload: Option<PrepareUnloadState>,
    /// Sticky Unload-preparation marker: set by `accept_prepare_unload` and never cleared, so
    /// every gate and rejection keeps its force until the actor is destroyed, even after the
    /// Idle settlement takes the `prepare_unload` state.  `prepare_unload` is only the
    /// deadline+waiters owner; `prepare_unload_accepted` is the lifecycle truth for "Unload
    /// preparation was accepted".
    prepare_unload_accepted: bool,
    security_invalidation: Option<SecurityInvalidationState>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

/// The latest intended availability facts observed while the Session was Starting/Running/
/// Finishing (or had an active admission/Turn).  It is never applied to a legal non-Idle
/// snapshot; the actor keeps only the latest intended value of every fact and applies the whole
/// composite when the Session returns to Idle, so a Disable followed by an Enable before
/// terminal collapses to the final value, and an Agent fact and a shared-resource reload fact
/// arriving in either order both survive to the single Idle application.  The timestamp and
/// command_id are always the last received command's, so the final `ReadinessChanged`
/// attribution is the command whose fact actually moved the public readiness last.
struct PendingAvailability {
    agent_available: bool,
    prompt_available: bool,
    model_available: bool,
    timestamp: Timestamp,
    command_id: CommandId,
}

/// The single accepted graceful-Unload preparation.  The admission gate is already closed by the
/// caller; the actor only keeps the shared deadline and waiters.  The effective deadline is the
/// earliest request's; a later request can never extend it, and a shorter deadline replaces it
/// while it has not yet fired.  Once `deadline_fired` is set, the main select disables the
/// deadline branch, so the fired deadline never re-triggers and a duplicate request only joins
/// the waiters without shortening or extending anything.  (The timer itself is re-created from
/// the copied deadline on every main-select iteration, so the state owns no pinned sleep.)  The
/// state is cleared when the actor becomes Idle; the sticky `prepare_unload_accepted` bool keeps
/// the admission gate closed and every gate/rejection in force until `close()`.
struct PrepareUnloadState {
    deadline: tokio::time::Instant,
    deadline_fired: bool,
    waiters: Vec<oneshot::Sender<Result<(), SessionExecutorPrepareUnloadError>>>,
}

/// The single accepted host security Workspace invalidation.  The admission gate is already
/// closed by the caller; the actor keeps the single sampled timestamp, the shared waiters, and
/// the owner-tracked recovery worker.  A duplicate invalidation only joins the waiters: it
/// never re-signals, never re-runs recovery, and never generates a CommandId.  The worker is
/// spawned at the first Idle recovery start point and reaped exactly once at its completion or
/// during close/reap; the waiters settle only after the recovery final state is installed (or
/// the executor closes/fails first).
struct SecurityInvalidationState {
    timestamp: Timestamp,
    waiters: Vec<oneshot::Sender<Result<(), SessionSecurityInvalidationError>>>,
    worker_task: Option<TrackedTask>,
}

struct ActivePublication {
    permit: SessionDefinitionPublicationPermit,
    expected: ExpectedPublication,
    timestamp: Timestamp,
    command_id: CommandId,
    waiter: Arc<PublicationWaiterState>,
    worker_task: Option<TrackedTask>,
}

struct ActiveAdmission {
    command_id: CommandId,
    turn_id: TurnId,
    emergency: crate::session_ingress::EmergencyControlObservation,
    intent: PromptIntent,
    waiters: Vec<oneshot::Sender<Result<TurnId, SessionSubmitError>>>,
    cancellation: CancellationToken,
    security_revocation: CancellationToken,
    cancel_accepted: Option<SessionCancelAccepted>,
    task: Option<TrackedTask>,
}

impl ActiveAdmission {
    fn settle(&mut self, outcome: Result<TurnId, SessionSubmitError>) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(outcome);
        }
    }
}

struct ActiveTurn {
    command_id: CommandId,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    emergency: crate::session_ingress::EmergencyControlObservation,
    cancellation: CancellationToken,
    cancel_accepted: Option<SessionCancelAccepted>,
    task: Option<TrackedTask>,
    steer_admission_open: bool,
    phase: TurnExecutionPhaseView,
}

struct ActiveInteraction {
    turn_id: TurnId,
    item_id: ItemId,
    resolution_sender: oneshot::Sender<ResolvedInteraction>,
}

#[derive(Clone)]
struct WorkspacePublicationContext {
    durable_state: DurableState,
    resolver: Arc<WorkspaceResolver>,
    prompt_service: Arc<PromptService>,
    executor_closing: CancellationToken,
    candidate_cancellation: CancellationToken,
}

impl WorkspacePublicationContext {
    fn is_cancelled(&self) -> bool {
        self.executor_closing.is_cancelled() || self.candidate_cancellation.is_cancelled()
    }

    async fn cancelled(&self) {
        tokio::select! {
            _ = self.executor_closing.cancelled() => {}
            _ = self.candidate_cancellation.cancelled() => {}
        }
    }
}

/// The one publication shape admitted by the loaded Session's single active publication slot:
/// an ordinary definition replacement, an explicit Agent revision upgrade, or a Workspace reload.
#[derive(Clone)]
enum ExpectedPublication {
    NoChange {
        definition: Arc<SessionDefinition>,
    },
    Publish {
        definition: Arc<SessionDefinition>,
        workspace_changed: bool,
    },
    AgentUpgrade {
        expected_revision: SessionDefinitionRevision,
    },
    ReloadWorkspace {
        definition: Arc<SessionDefinition>,
    },
}

impl ExpectedPublication {
    fn definition(&self) -> &Arc<SessionDefinition> {
        match self {
            Self::NoChange { definition }
            | Self::Publish { definition, .. }
            | Self::ReloadWorkspace { definition } => definition,
            Self::AgentUpgrade { .. } => {
                panic!("an Agent upgrade publication never carries a prebuilt candidate")
            }
        }
    }

    const fn is_publish(&self) -> bool {
        matches!(self, Self::Publish { .. })
    }

    const fn workspace_changed(&self) -> bool {
        match self {
            Self::NoChange { .. } | Self::AgentUpgrade { .. } | Self::ReloadWorkspace { .. } => {
                false
            }
            Self::Publish {
                workspace_changed, ..
            } => *workspace_changed,
        }
    }
}

#[derive(Clone, Copy)]
enum ActorFatality {
    Integrity,
    Internal,
}

/// One select iteration of the actor main loop.  The branches borrow disjoint fields, so the
/// wakeup is collected first and processed after the select completes.
enum ActorWakeup {
    Closing,
    PrepareDeadline,
    Completion(ExecutorCompletion),
    Request(SessionExecutorRequest),
}

/// Waits until the copied graceful-Unload deadline.  The timer is re-created on every main-select
/// iteration, so a request that shortens the effective deadline is picked up by the next
/// iteration's copied value; a fired deadline never re-fires because the handler marks the state
/// `deadline_fired` and the main select disables the deadline branch from then on.
async fn prepare_unload_deadline(deadline: Option<tokio::time::Instant>) {
    let Some(deadline) = deadline else {
        return;
    };
    tokio::time::sleep_until(deadline).await;
}

impl SessionExecutorActor {
    async fn run(mut self) -> bool {
        loop {
            if self.closing.is_cancelled() {
                return self.close_and_drain().await;
            }
            let prepare_deadline = self.prepare_unload.as_ref().and_then(|state| {
                if state.deadline_fired {
                    None
                } else {
                    Some(state.deadline)
                }
            });
            let wakeup = tokio::select! {
                biased;
                _ = self.closing.cancelled() => ActorWakeup::Closing,
                _ = prepare_unload_deadline(prepare_deadline), if prepare_deadline.is_some() => ActorWakeup::PrepareDeadline,
                completion = self.completions.recv() => match completion {
                    Some(completion) => ActorWakeup::Completion(completion),
                    None => {
                        self.reap_after_missing_completion().await;
                        return self.close_and_drain().await;
                    }
                },
                request = self.emergency_receiver.recv() => match request {
                    Some(request) => ActorWakeup::Request(request),
                    None => return self.close_and_drain().await,
                },
                request = self.receiver.recv() => match request {
                    Some(request) => ActorWakeup::Request(request),
                    None => return self.close_and_drain().await,
                },
            };
            match wakeup {
                ActorWakeup::Closing => return self.close_and_drain().await,
                ActorWakeup::PrepareDeadline => {
                    if let Err(fatality) = self.handle_prepare_deadline().await {
                        self.close_for_fatal(fatality);
                        return self.close_and_drain().await;
                    }
                }
                ActorWakeup::Completion(completion) => {
                    if let Err(fatality) = self.handle_completion(completion).await {
                        self.close_for_fatal(fatality);
                        return self.close_and_drain().await;
                    }
                }
                ActorWakeup::Request(mut request) => {
                    if self.closing.is_cancelled() {
                        request.reject_closing();
                        continue;
                    }
                    if let Err(fatality) = self.handle_request(&mut request).await {
                        self.close_for_fatal(fatality);
                        return self.close_and_drain().await;
                    }
                }
            }
        }
    }

    async fn close_and_drain(&mut self) -> bool {
        self.receiver.close();
        self.emergency_receiver.close();
        self.follow_up.clear();
        self.steer.clear();
        if let Some(state) = self.prepare_unload.take() {
            // Distinguish a fatal exit (Internal) from an ordinary close (Closing) so the
            // unload caller maps the outcome truthfully.
            let outcome = if self.failure_state.is_fatal() {
                Err(SessionExecutorPrepareUnloadError::Internal)
            } else {
                Err(SessionExecutorPrepareUnloadError::Closing)
            };
            for waiter in state.waiters {
                let _ = waiter.send(outcome);
            }
        }
        // A registered security invalidation without a running worker settles exactly once
        // here (ordinary close -> Closing, fatal -> Internal); a running worker's completion
        // is handled by the drain loop below, which settles its waiters through
        // `handle_security_recovery_completion` before this function returns, so close never
        // leaves a worker/task behind.
        if let Some(state) = self.security_invalidation.take() {
            if state.worker_task.is_some() {
                self.security_invalidation = Some(state);
            } else {
                let outcome = if self.failure_state.is_fatal() {
                    Err(SessionSecurityInvalidationError::InternalDispatchUnavailable)
                } else {
                    Err(SessionSecurityInvalidationError::Closing)
                };
                for waiter in state.waiters {
                    let _ = waiter.send(outcome);
                }
            }
        }
        // Only an executor that still has an active admission or Turn projects Finishing.  An
        // already-Idle prepared executor must not fabricate a Finishing ExecutionChanged event.
        if self.active_admission.is_some() || self.active_turn.is_some() {
            self.execution_state = SessionExecutionState::Finishing;
            self.install_current_state(SessionExecutionState::Finishing);
        }
        let pending_interactions = self
            .pending_interactions
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut requests_drained = false;
        let mut emergency_requests_drained = false;
        let mut normal_exit = true;
        if !pending_interactions.is_empty() {
            if let Some(active_turn) = self.active_turn.as_ref() {
                active_turn.cancellation.cancel();
                if self
                    .signal_emergency_cancel(active_turn.emergency.target())
                    .is_err()
                {
                    normal_exit = false;
                }
            }
        }
        for request_id in pending_interactions {
            if self
                .cancel_pending_interaction(
                    request_id,
                    SystemClock.now(),
                    InteractionCancelReason::SessionUnloaded,
                )
                .await
                .is_err()
            {
                normal_exit = false;
            }
        }

        loop {
            if !requests_drained {
                loop {
                    match self.receiver.try_recv() {
                        Ok(mut request) => request.reject_closing(),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            requests_drained = true;
                            break;
                        }
                    }
                }
            }
            if !emergency_requests_drained {
                loop {
                    match self.emergency_receiver.try_recv() {
                        Ok(mut request) => request.reject_closing(),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            emergency_requests_drained = true;
                            break;
                        }
                    }
                }
            }

            if self.active_publication.is_none()
                && self.active_admission.is_none()
                && self.active_turn.is_none()
                && !self.security_recovery_is_active()
                && requests_drained
                && emergency_requests_drained
            {
                if let Some(conversation) = &self.conversation {
                    conversation.recorder.close().await;
                }
                return normal_exit;
            }

            tokio::select! {
                biased;
                completion = self.completions.recv(), if self.active_publication.is_some() || self.active_admission.is_some() || self.active_turn.is_some() || self.security_recovery_is_active() => match completion {
                    Some(completion) => {
                        if let Err(fatality) = self.handle_completion(completion).await {
                            normal_exit = false;
                            self.close_for_fatal(fatality);
                        }
                    }
                    None => {
                        normal_exit = false;
                        self.reap_after_missing_completion().await;
                    }
                },
                request = self.receiver.recv(), if !requests_drained => match request {
                    Some(mut request) => request.reject_closing(),
                    None => requests_drained = true,
                },
                request = self.emergency_receiver.recv(), if !emergency_requests_drained => match request {
                    Some(mut request) => request.reject_closing(),
                    None => emergency_requests_drained = true,
                },
            }
        }
    }

    async fn reap_after_missing_completion(&mut self) {
        // The completions channel closed while work was active: settle and reap every active
        // owner exactly once, then the caller drains.  A running security recovery worker is
        // reaped and its waiters settle Internal; the invalidation state is cleared so the
        // drain loop cannot wait for a completion that can never arrive.
        let mut security_reaped = false;
        if let Some(mut state) = self.security_invalidation.take() {
            security_reaped = true;
            if let Some(worker_task) = state.worker_task.take() {
                let _ = worker_task.wait().await;
            }
            for waiter in state.waiters {
                let _ = waiter.send(Err(
                    SessionSecurityInvalidationError::InternalDispatchUnavailable,
                ));
            }
        }
        let Some(mut active) = self.active_publication.take() else {
            if security_reaped {
                self.turn_admission_gate.close();
                self.closing.cancel();
                self.failure_state.mark_fatal();
                self.task_context.request_closing();
                self.durable_state.request_closing();
            }
            return;
        };
        if let Some(worker_task) = active.worker_task.take() {
            let _ = worker_task.wait().await;
        }
        self.turn_admission_gate.close();
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        active.waiter.settle(Err(
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
        ));
        self.finish_active_waiter(&active.waiter);
    }

    async fn handle_request(
        &mut self,
        request: &mut SessionExecutorRequest,
    ) -> Result<(), ActorFatality> {
        match request {
            SessionExecutorRequest::Snapshot(request) => {
                #[cfg(test)]
                self.hooks.before_snapshot_response().await;
                request.settle(Ok(Arc::clone(&self.current)));
            }
            SessionExecutorRequest::PublishMetadata(request) => {
                self.publish_metadata(request)?;
            }
            SessionExecutorRequest::Subscribe(request) => {
                request.settle(Ok(SessionExecutorSubscription {
                    snapshot: Arc::clone(&self.current),
                    receiver: self.events.subscribe(),
                }));
            }
            #[cfg(test)]
            SessionExecutorRequest::StartingProbe(request) => {
                if self.active_publication.is_some() || !self.execution_state.is_idle() {
                    request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
                } else {
                    request.settle(Ok(()));
                }
            }
            SessionExecutorRequest::Update(request) => {
                self.start_publication(request)?;
            }
            SessionExecutorRequest::UpgradeAgent(request) => {
                self.start_agent_upgrade_publication(request)?;
            }
            SessionExecutorRequest::ReloadWorkspace(request) => {
                self.start_workspace_reload(request)?;
            }
            SessionExecutorRequest::Submit(request) => {
                self.start_admission(request)?;
            }
            SessionExecutorRequest::FollowUp(request) => {
                self.enqueue_follow_up(request)?;
            }
            SessionExecutorRequest::Steer(request) => {
                self.enqueue_steer(request)?;
            }
            SessionExecutorRequest::CancelQueuedMessage(request) => {
                self.cancel_queued_message_request(request)?;
            }
            SessionExecutorRequest::ResolveInteraction(request) => {
                self.resolve_interaction_request(request).await?;
            }
            SessionExecutorRequest::Cancel(request) => {
                self.cancel_request(request).await?;
            }
            SessionExecutorRequest::SecurityRevoked(request) => {
                self.security_revoke_request(request).await?;
            }
            SessionExecutorRequest::SecurityInvalidation(request) => {
                self.begin_security_invalidation(request).await?;
            }
            SessionExecutorRequest::SetAgentAvailability(request) => {
                self.set_agent_availability(request)?;
            }
            SessionExecutorRequest::UpdateSharedResources(request) => {
                self.update_shared_resources(request)?;
            }
            SessionExecutorRequest::PrepareUnload(request) => {
                self.accept_prepare_unload(request)?;
            }
        }
        Ok(())
    }

    /// Accepts one graceful-Unload preparation.  Admission is closed (the caller already closed
    /// the gate synchronously; this keeps it closed), the actor Steer and FollowUp lanes are
    /// cleared and re-projected, and the request joins the shared deadline state: the effective
    /// deadline only shortens while it has not fired, and an already-Idle executor settles
    /// immediately.  Once the deadline has fired, a duplicate request only joins the waiters.
    /// The sticky `prepare_unload_accepted` marker is set here first and never cleared, so the
    /// admission gate stays closed and every gate/rejection (Submit, Steer, FollowUp, security
    /// invalidation, recovery-completion reopen, Turn-terminal FollowUp handoff) keeps its
    /// force until the actor is destroyed, even after the Idle settlement takes the deadline
    /// state.  The actor keeps running until `close()`; new Submit/Steer/FollowUp requests are
    /// rejected from now on, while ResolveInteraction, Cancel, SecurityRevoked, Snapshot, and
    /// accepted publication completions remain processable.
    fn accept_prepare_unload(
        &mut self,
        request: &mut PrepareUnloadRequest,
    ) -> Result<(), ActorFatality> {
        self.prepare_unload_accepted = true;
        self.turn_admission_gate.close();
        self.follow_up.clear();
        self.steer.clear();
        self.publish_queue_projection();
        let Some(response) = request.response.take() else {
            return Err(ActorFatality::Integrity);
        };
        match self.prepare_unload.as_mut() {
            Some(state) => {
                // Once the deadline has fired, a duplicate request only joins the waiters: it
                // neither re-triggers the handler nor shortens/extends the effective deadline.
                // Before the deadline fires, the effective deadline can only shorten.
                if !state.deadline_fired && request.deadline < state.deadline {
                    state.deadline = request.deadline;
                }
                state.waiters.push(response);
            }
            None => {
                self.prepare_unload = Some(PrepareUnloadState {
                    deadline: request.deadline,
                    deadline_fired: false,
                    waiters: vec![response],
                });
            }
        }
        self.settle_prepare_unload_if_idle();
        Ok(())
    }

    /// The graceful-Unload deadline fired while the actor still has an active admission or Turn.
    /// The exact current emergency target is signaled `PrepareForUnload` first-wins (an earlier
    /// Cancel/SecurityRevoked keeps its original signal and reason), the cancellation token is
    /// cancelled, and the Turn's pending Interactions are truthfully settled as
    /// `SessionUnloaded`.  Only an active Turn projects Finishing (the first such projection
    /// emits `ExecutionChanged`); an admission-only deadline keeps `Starting` — and its Starting
    /// submit queue projection — legal, leaving the truth to the admission completion, which
    /// either reclassifies the failed admission to `PrepareForUnload` and returns to Idle, or
    /// migrates the signal onto the same Turn and then projects Finishing and settles terminal.
    /// Active tasks are never dropped: they settle through the same completion paths as
    /// ordinary emergency control.  The deadline fires at most once per preparation: this
    /// handler marks the state `deadline_fired`, so the main select disables the deadline branch
    /// and a duplicate Prepare only joins the waiters from now on.
    async fn handle_prepare_deadline(&mut self) -> Result<(), ActorFatality> {
        if self.prepare_unload.is_none() {
            return Ok(());
        }
        // The deadline has fired.  The main select's deadline branch is disabled from this
        // point, so the handler never re-runs for this preparation and no re-armed timer is
        // needed while the admission/Turn/publication settlement drains.
        self.prepare_unload
            .as_mut()
            .expect("prepare state exists after the guard")
            .deadline_fired = true;
        let target = if let Some(active) = self.active_admission.as_ref() {
            Some(active.emergency.target())
        } else if let Some(active) = self.active_turn.as_ref() {
            Some(active.emergency.target())
        } else {
            None
        };
        if let Some(target) = target {
            let prepared_won = match self
                .emergency
                .signal(target, EmergencyControlSignal::PrepareForUnload)
            {
                EmergencyControlSignalOutcome::Accepted { .. } => true,
                // An earlier Cancel or SecurityRevoked keeps its original signal and reason.
                EmergencyControlSignalOutcome::AlreadySignaled { .. } => false,
                EmergencyControlSignalOutcome::StaleTarget => return Err(ActorFatality::Integrity),
            };
            if prepared_won {
                if let Some(active) = self.active_admission.as_ref() {
                    active.cancellation.cancel();
                }
                if let Some(active) = self.active_turn.as_ref() {
                    active.cancellation.cancel();
                }
                // Only an active Turn projects Finishing.  An admission-only deadline (the
                // Input is still being captured) keeps Starting and its Starting submit queue
                // projection legal: the admission completion settles the truth, either by
                // reclassifying the failed admission to PrepareForUnload and returning to Idle,
                // or by migrating the signal onto the same Turn, which then projects Finishing.
                if self.active_turn.is_some()
                    && self.execution_state != SessionExecutionState::Finishing
                {
                    self.execution_state = SessionExecutionState::Finishing;
                    self.publish_execution_state(
                        SessionExecutionState::Finishing,
                        self.current.current_turn(),
                        self.current.last_terminal(),
                    );
                }
                // Pending Interactions exist only while a Turn is active, so an admission-only
                // deadline finds this map empty.
                let pending = self.pending_interactions.keys().copied().collect::<Vec<_>>();
                for request_id in pending {
                    self.cancel_pending_interaction(
                        request_id,
                        SystemClock.now(),
                        InteractionCancelReason::SessionUnloaded,
                    )
                    .await?;
                }
            }
        } else {
            // No active admission or Turn at the deadline: nothing to signal.  The waiters
            // settle now when the executor is fully Idle (no active publication either).
            self.settle_prepare_unload_if_idle();
        }
        Ok(())
    }

    /// Settles every accepted graceful-Unload waiter with Ok only when no active publication,
    /// admission, or Turn remains.  The state is cleared, but the sticky
    /// `prepare_unload_accepted` marker keeps the admission gate closed and every
    /// gate/rejection in force until `close()`.
    ///
    /// A still-running security recovery worker is deliberately not a blocker here: `prepare`
    /// must not wait for the recovery to finish, and `close()` is responsible for cancelling
    /// and reaping it.  The recovery's resolve/capture/revalidate awaits are all
    /// cancellation-aware against the executor closing token, so once prepare settles,
    /// `close()` cancels that token and `close_and_drain` reaps the worker and settles its
    /// waiters truthfully with Closing (or Internal on a fatal path), while the sticky
    /// `prepare_unload_accepted` marker in the recovery completion handler keeps the admission
    /// gate closed during the preparation.
    fn settle_prepare_unload_if_idle(&mut self) {
        if self.active_publication.is_some()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
        {
            return;
        }
        let Some(state) = self.prepare_unload.take() else {
            return;
        };
        for waiter in state.waiters {
            let _ = waiter.send(Ok(()));
        }
    }

    fn enqueue_follow_up(&mut self, request: &mut FollowUpRequest) -> Result<(), ActorFatality> {
        if self.prepare_unload_accepted {
            request.settle(Err(SessionFollowUpError::TurnNotRunning));
            return Ok(());
        }
        let Some(active_turn) = self.active_turn.as_ref() else {
            request.settle(Err(SessionFollowUpError::TurnNotRunning));
            return Ok(());
        };
        if active_turn.command_id == request.command_id
            || self
                .active_admission
                .as_ref()
                .is_some_and(|admission| admission.command_id == request.command_id)
            || self.steer.contains(request.command_id)
        {
            request.settle(Err(SessionFollowUpError::CommandConflict));
            return Ok(());
        }
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        match self.follow_up.try_push(request.command_id, intent) {
            Ok(()) => {
                self.publish_queue_projection();
                request.settle(Ok(()));
            }
            Err(FollowUpQueueError::Full) => {
                request.settle(Err(SessionFollowUpError::QueueFull));
            }
            Err(FollowUpQueueError::DuplicateCommandId) => {
                request.settle(Err(SessionFollowUpError::CommandConflict));
            }
        }
        Ok(())
    }

    fn cancel_queued_message_request(
        &mut self,
        request: &mut CancelQueuedMessageRequest,
    ) -> Result<(), ActorFatality> {
        let removed = self.steer.remove(request.command_id).is_some()
            || self.follow_up.remove(request.command_id).is_some();
        if removed {
            self.publish_queue_projection();
            request.settle(Ok(()));
        } else {
            request.settle(Err(SessionQueuedMessageError::NotQueued));
        }
        Ok(())
    }

    fn enqueue_steer(&mut self, request: &mut SteerRequest) -> Result<(), ActorFatality> {
        if self.prepare_unload_accepted {
            request.settle(Err(SessionSteerError::TurnNotRunning));
            return Ok(());
        }
        let Some(active_turn) = self.active_turn.as_ref() else {
            request.settle(Err(SessionSteerError::TurnNotRunning));
            return Ok(());
        };
        if active_turn.turn_id != request.turn_id {
            request.settle(Err(SessionSteerError::ExpectedTurnMismatch));
            return Ok(());
        }
        if active_turn.cancellation.is_cancelled() {
            request.settle(Err(SessionSteerError::TurnCancelling));
            return Ok(());
        }
        if !active_turn.steer_admission_open {
            request.settle(Err(SessionSteerError::TurnNotRunning));
            return Ok(());
        }
        if active_turn.command_id == request.command_id
            || self
                .active_admission
                .as_ref()
                .is_some_and(|admission| admission.command_id == request.command_id)
            || self.follow_up.contains(request.command_id)
        {
            request.settle(Err(SessionSteerError::CommandConflict));
            return Ok(());
        }
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        match self
            .steer
            .try_push(request.turn_id, request.command_id, intent)
        {
            Ok(()) => {
                self.publish_queue_projection();
                request.settle(Ok(()));
            }
            Err(SteerQueueError::Full) => request.settle(Err(SessionSteerError::QueueFull)),
            Err(SteerQueueError::DuplicateCommandId) => {
                request.settle(Err(SessionSteerError::CommandConflict));
            }
        }
        Ok(())
    }

    /// Applies one Agent availability fact.  The installed definition must pin the requested
    /// AgentId; any mismatch is an internal invariant that poisons the executor.  An Idle
    /// Session with no active admission/Turn applies the fact immediately and publishes
    /// `ReadinessChanged` only when the public readiness truly changes; any other Session merges
    /// the fact into the latest intended availability composite (a Disable followed by an Enable
    /// collapses to the final value, and an interleaved shared-resource reload fact survives to
    /// the same Idle application) and applies it when it returns to Idle.  Every applied fact
    /// re-projects the public queue under the new readiness first, so the retained internal
    /// FollowUp is hidden while AgentUnavailable and re-exposed on Enable, and the
    /// `ReadinessChanged` snapshot carries the re-projected queue.
    fn set_agent_availability(
        &mut self,
        request: &mut AgentAvailabilityRequest,
    ) -> Result<(), ActorFatality> {
        if self.current.definition().agent().agent_id() != request.agent_id {
            request.settle(Err(SessionAgentAvailabilityError::InternalDispatchUnavailable));
            return Err(ActorFatality::Integrity);
        }
        if self.execution_state.is_idle()
            && self.active_admission.is_none()
            && self.active_turn.is_none()
        {
            if self.current.agent_available != request.available {
                let previous = self.current.readiness();
                self.publish_current(Arc::new(self.current.with_agent_availability(request.available)));
                // Re-project the public queue under the new readiness before any comparison or
                // event, so an AgentDisable hides the retained internal FollowUp and an Enable
                // re-exposes it; the ReadinessChanged snapshot must carry this projection.
                self.publish_queue_projection();
                if self.current.readiness() != previous {
                    let _ = self
                        .events
                        .send(Arc::new(SessionExecutorEvent::ReadinessChanged {
                            timestamp: request.timestamp,
                            command_id: Some(request.command_id),
                            snapshot: Arc::clone(&self.current),
                        }));
                }
                request.settle(Ok(()));
                if request.available {
                    self.start_queued_follow_up_after_publication()?;
                }
            } else {
                request.settle(Ok(()));
            }
            return Ok(());
        }
        self.merge_pending_availability(
            request.available,
            self.pending_availability
                .as_ref()
                .map(|pending| pending.prompt_available)
                .unwrap_or(self.current.prompt_available),
            self.pending_availability
                .as_ref()
                .map(|pending| pending.model_available)
                .unwrap_or(self.current.model_available),
            request.timestamp,
            request.command_id,
        );
        request.settle(Ok(()));
        Ok(())
    }

    /// Installs one Runtime shared-resource pair on the loaded Session and applies the
    /// precomputed model/selected-Prompt availability facts of the exact installed definition
    /// against the candidate resources.  The actor validates by `Arc::ptr_eq` that the exact
    /// definition the Runtime precomputed against is still installed; any mismatch is an
    /// internal invariant that poisons the executor.  The future `turn_resources` roots are
    /// replaced immediately (preserving the model gateway, tool set, and compaction settings),
    /// so an active Turn keeps its already-captured old context, a FollowUp admitted before this
    /// request linearizes before the reload and keeps its old capture, and a FollowUp admitted
    /// after this request uses the new roots.  An Idle Session with no active admission/Turn
    /// applies the facts immediately and publishes `ReadinessChanged` only when the public
    /// readiness truly changes; any other Session merges them into the latest intended
    /// availability composite and applies them when it returns to Idle.
    fn update_shared_resources(
        &mut self,
        request: &mut SharedResourceUpdateRequest,
    ) -> Result<(), ActorFatality> {
        if !Arc::ptr_eq(self.current.definition(), &request.expected_definition) {
            request.settle(Err(SessionSharedResourceUpdateError::InternalDispatchUnavailable));
            return Err(ActorFatality::Integrity);
        }
        let Some(resources) = self.turn_resources.as_mut() else {
            request.settle(Err(SessionSharedResourceUpdateError::InternalDispatchUnavailable));
            return Err(ActorFatality::Integrity);
        };
        resources.prompt_resources = Arc::clone(&request.prompt_resources);
        resources.model_catalog = Arc::clone(&request.model_catalog);
        if self.execution_state.is_idle()
            && self.active_admission.is_none()
            && self.active_turn.is_none()
        {
            if self.current.prompt_available != request.prompt_available
                || self.current.model_available != request.model_available
            {
                let previous = self.current.readiness();
                self.publish_current(Arc::new(self.current.with_combined_availability(
                    self.current.agent_available,
                    request.model_available,
                    request.prompt_available,
                )));
                // Re-project the public queue under the new readiness before any comparison or
                // event, so an unavailable selected Prompt/model hides the retained internal
                // FollowUp and a restore re-exposes it; the ReadinessChanged snapshot must
                // carry this projection.
                self.publish_queue_projection();
                if self.current.readiness() != previous {
                    let _ = self
                        .events
                        .send(Arc::new(SessionExecutorEvent::ReadinessChanged {
                            timestamp: request.timestamp,
                            command_id: Some(request.command_id),
                            snapshot: Arc::clone(&self.current),
                        }));
                }
            }
            request.settle(Ok(()));
            self.start_queued_follow_up_after_publication()?;
            return Ok(());
        }
        self.merge_pending_availability(
            self.pending_availability
                .as_ref()
                .map(|pending| pending.agent_available)
                .unwrap_or(self.current.agent_available),
            request.prompt_available,
            request.model_available,
            request.timestamp,
            request.command_id,
        );
        request.settle(Ok(()));
        Ok(())
    }

    /// Merges one observed availability fact set into the latest intended composite.  The
    /// composite never exists on a legal Idle-without-admission/Turn snapshot, so every caller
    /// stores the current installed value of the facts it does not update.
    fn merge_pending_availability(
        &mut self,
        agent_available: bool,
        prompt_available: bool,
        model_available: bool,
        timestamp: Timestamp,
        command_id: CommandId,
    ) {
        self.pending_availability = Some(PendingAvailability {
            agent_available,
            prompt_available,
            model_available,
            timestamp,
            command_id,
        });
    }

    /// Applies the latest intended availability composite after the Session has returned to
    /// Idle and publishes `ReadinessChanged` only when the public readiness truly changed.  The
    /// new readiness is projected onto the public queue before the comparison, so the retained
    /// internal FollowUp is hidden while Unavailable and re-exposed on restore, and the
    /// `ReadinessChanged` snapshot carries the re-projected queue.  The attribution is the last
    /// command that observed a fact.  The caller decides the FollowUp handoff: an Enable applied
    /// here leaves one queued FollowUp for the terminal path's pop/start or for the next Enable,
    /// so a retained FollowUp is never dropped.
    fn apply_pending_availability(&mut self) -> Result<(), ActorFatality> {
        let Some(pending) = self.pending_availability.take() else {
            return Ok(());
        };
        let previous = self.current.readiness();
        self.publish_current(Arc::new(self.current.with_combined_availability(
            pending.agent_available,
            pending.model_available,
            pending.prompt_available,
        )));
        // Re-project the public queue under the new readiness before any comparison or event,
        // so the retained internal FollowUp is hidden while Unavailable and re-exposed on
        // restore; the ReadinessChanged snapshot must carry this projection.
        self.publish_queue_projection();
        if self.current.readiness() != previous {
            let _ = self
                .events
                .send(Arc::new(SessionExecutorEvent::ReadinessChanged {
                    timestamp: pending.timestamp,
                    command_id: Some(pending.command_id),
                    snapshot: Arc::clone(&self.current),
                }));
        }
        Ok(())
    }

    fn start_admission(&mut self, request: &mut SubmitRequest) -> Result<(), ActorFatality> {
        if self.prepare_unload_accepted {
            // The admission gate is closed and the queues were cleared at PrepareForUnload
            // accept; this Submit is rejected exactly like a closed admission gate.  The
            // sticky marker also rejects Submits that were already in the bounded lane and
            // are handled after the preparation settled (the state was taken), so a queued
            // Submit can never bypass the preparation.
            request.settle(Err(SessionSubmitError::Closing));
            return Ok(());
        }
        // Readiness is checked before any TurnId generation, execution mutation, or Workspace
        // clone.  An Unavailable Session is always Idle with no active admission/Turn/queues,
        // so this settles before the SessionBusy path and before any Ready-only accessor.  A
        // security-invalidation Preparing Session follows the existing RuntimeDependency
        // fallback contract: `SessionNotReady(RuntimeDependencyUnavailable)` maps to
        // `SessionNotReady` + RetryWithBackoff publicly.
        match self.current.readiness() {
            SessionReadinessView::Ready => {}
            SessionReadinessView::Unavailable(cause) => {
                request.settle(Err(SessionSubmitError::SessionNotReady(cause)));
                return Ok(());
            }
            SessionReadinessView::Preparing => {
                request.settle(Err(SessionSubmitError::SessionNotReady(
                    SessionUnavailableView::RuntimeDependencyUnavailable,
                )));
                return Ok(());
            }
        }
        if let Some(active) = self.active_admission.as_mut() {
            if active.command_id == request.command_id {
                let Some(intent) = request.intent.as_ref() else {
                    return Err(ActorFatality::Integrity);
                };
                if intent != &active.intent {
                    request.settle(Err(SessionSubmitError::CommandConflict));
                    return Ok(());
                }
                let Some(waiter) = request.response.take() else {
                    return Err(ActorFatality::Integrity);
                };
                active.waiters.push(waiter);
                request.intent.take();
                return Ok(());
            }
        }
        if self.active_publication.is_some()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
            || !self.execution_state.is_idle()
        {
            request.settle(Err(SessionSubmitError::SessionBusy));
            return Ok(());
        }
        let (Some(conversation), Some(resources)) =
            (self.conversation.clone(), self.turn_resources.clone())
        else {
            request.settle(Err(SessionSubmitError::DependencyUnavailable));
            return Ok(());
        };
        let Some(intent) = request.intent.take() else {
            return Err(ActorFatality::Integrity);
        };
        let admission_intent = intent.clone();
        let turn_id = TurnId::generate().map_err(|_| ActorFatality::Internal)?;
        let command_id = request.command_id;
        let waiters = request.response.take().into_iter().collect();
        let emergency = self
            .emergency
            .bind(EmergencyControlTarget::Submit(command_id))
            .map_err(|_| ActorFatality::Internal)?;
        self.active_admission = Some(ActiveAdmission {
            command_id,
            turn_id,
            emergency,
            intent: admission_intent,
            waiters,
            cancellation: CancellationToken::new(),
            security_revocation: CancellationToken::new(),
            cancel_accepted: None,
            task: None,
        });
        self.execution_state = SessionExecutionState::Starting;
        self.publish_execution_state(
            SessionExecutionState::Starting,
            None,
            self.current.last_terminal(),
        );

        let cancellation = self
            .active_admission
            .as_ref()
            .expect("admission is installed before spawning")
            .cancellation
            .clone();
        let security_revocation = self
            .active_admission
            .as_ref()
            .expect("admission is installed before spawning")
            .security_revocation
            .clone();

        let completion_sender = self.completion_sender.clone();
        let durable_state = self.durable_state.clone();
        let definition = Arc::clone(self.current.definition());
        let workspace = Arc::clone(self.current.workspace());
        let prompt_service = Arc::clone(&self.prompt_service);
        let closing = self.closing.clone();
        let turn_admission_gate = Arc::clone(&self.turn_admission_gate);
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        let guard = AdmissionCompletionGuard::new(completion_sender, turn_id);
        let worker = async move {
            let mut guard = guard;
            let result = run_admission(AdmissionWork {
                closing,
                durable_state,
                definition,
                workspace,
                prompt_service,
                resources,
                conversation,
                turn_admission_gate,
                cancellation,
                security_revocation,
                turn_id,
                intent,
                #[cfg(test)]
                hooks,
            })
            .await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_admission
                    .as_mut()
                    .expect("admission is installed before spawn")
                    .task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {}
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                self.task_context.request_closing();
                self.durable_state.request_closing();
            }
        }
        Ok(())
    }

    async fn handle_admission_completion(
        &mut self,
        completion: AdmissionCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_admission.take() else {
            return Err(ActorFatality::Internal);
        };
        let task_result = match active.task.take() {
            Some(task) => task.wait().await,
            None => Ok(()),
        };
        if task_result.is_err() || active.turn_id != completion.turn_id {
            active.settle(Err(SessionSubmitError::InternalDispatchUnavailable));
            return Err(ActorFatality::Internal);
        }

        let context = match completion.result {
            Ok(context) => context,
            Err(error) => {
                // Observe the current emergency signal before retiring the admission
                // observation: a PrepareForUnload that won the deadline reclassifies the
                // generic admission cancellation (a user Cancel still stays SubmitCancelled).
                // Every other failure stays truthful.
                let emergency_signal = self
                    .emergency
                    .observe(active.emergency.target())
                    .and_then(|observation| observation.signal());
                self.emergency.retire(active.emergency);
                let error = match (emergency_signal, error) {
                    (
                        Some(EmergencyControlSignal::PrepareForUnload),
                        SessionSubmitError::Cancelled | SessionSubmitError::Closing,
                    ) => SessionSubmitError::PrepareForUnload,
                    _ => error,
                };
                self.execution_state = SessionExecutionState::Idle;
                self.publish_execution_state(
                    SessionExecutionState::Idle,
                    None,
                    self.current.last_terminal(),
                );
                // The Session is Idle again: apply the latest intended availability composite
                // so the readiness projected by this failure is the final value.
                self.apply_pending_availability()?;
                active.settle(Err(error));
                self.settle_prepare_unload_if_idle();
                // A security invalidation that signaled this admission starts its Workspace
                // recovery now that the admission failure cleanup is complete.
                self.start_security_recovery_worker()?;
                return Ok(());
            }
        };

        let Some(conversation) = self.conversation.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        let Some(resources) = self.turn_resources.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        let prior_signal = self
            .emergency
            .observe(active.emergency.target())
            .and_then(|observation| observation.signal());
        self.emergency.retire(active.emergency);
        let emergency = self
            .emergency
            .bind(EmergencyControlTarget::Turn(active.turn_id))
            .map_err(|_| ActorFatality::Internal)?;
        let cancellation = active.cancellation.clone();
        if let Some(signal) = prior_signal {
            self.signal_emergency(EmergencyControlTarget::Turn(active.turn_id), signal)?;
            cancellation.cancel();
        } else if cancellation.is_cancelled() {
            self.signal_emergency_cancel(EmergencyControlTarget::Turn(active.turn_id))?;
        }
        let control_generation = Arc::new(ControlGeneration(0));
        conversation.install_control_generation(active.turn_id, Arc::clone(&control_generation));
        let finishing = prior_signal.is_some() || active.cancel_accepted.is_some();
        self.execution_state = if finishing {
            SessionExecutionState::Finishing
        } else {
            SessionExecutionState::Running
        };
        self.publish_execution_state(
            self.execution_state,
            Some(active.turn_id),
            self.current.last_terminal(),
        );
        self.active_turn = Some(ActiveTurn {
            command_id: active.command_id,
            turn_id: active.turn_id,
            control_generation: Arc::clone(&control_generation),
            emergency,
            cancellation: cancellation.clone(),
            cancel_accepted: active.cancel_accepted,
            task: None,
            steer_admission_open: true,
            phase: TurnExecutionPhaseView::Sampling,
        });

        let turn_id = active.turn_id;
        let emergency_control = self.emergency.clone();
        let emergency_observation = emergency;
        if let Some(signal) = prior_signal {
            let _ = self
                .completion_sender
                .send(ExecutorCompletion::Turn(TurnCompletion {
                    turn_id,
                    terminal: SessionTurnTerminal::Interrupted(session_turn_interruption(signal)),
                }));
        } else if cancellation.is_cancelled() {
            let _ = self
                .completion_sender
                .send(ExecutorCompletion::Turn(TurnCompletion {
                    turn_id,
                    terminal: SessionTurnTerminal::Failed(SessionTurnFailure::Model),
                }));
        } else {
            let completion_sender = self.completion_sender.clone();
            let executor_closing = self.closing.clone();
            let lifecycle_closing = self.lifecycle_closing.clone();
            let guard = TurnCompletionGuard::new(completion_sender, turn_id);
            let interaction_completion_sender = self.completion_sender.clone();
            let steer_completion_sender = self.completion_sender.clone();
            #[cfg(test)]
            let hooks = Arc::clone(&self.hooks);
            let worker = async move {
                let mut guard = guard;
                let terminal = run_active_turn(
                    context,
                    resources.model_gateway,
                    conversation,
                    turn_id,
                    control_generation,
                    emergency_control,
                    emergency_observation,
                    cancellation,
                    executor_closing,
                    lifecycle_closing,
                    interaction_completion_sender,
                    steer_completion_sender,
                    #[cfg(test)]
                    hooks,
                )
                .await;
                guard.complete(terminal);
            };
            match self.task_context.spawn_tracked(worker) {
                Ok(task) => {
                    self.active_turn
                        .as_mut()
                        .expect("active Turn is installed before spawn")
                        .task = Some(task);
                }
                Err(RuntimeTaskError::OwnerClosing) => {}
                Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                    self.task_context.request_closing();
                    self.durable_state.request_closing();
                }
            }
        }
        active.settle(Ok(active.turn_id));
        self.settle_prepare_unload_if_idle();
        Ok(())
    }

    async fn handle_interaction_requested(
        &mut self,
        completion: InteractionRequestedCompletion,
    ) -> Result<(), ActorFatality> {
        if self.closing.is_cancelled() {
            return Ok(());
        }
        let Some(active_turn) = self.active_turn.as_ref() else {
            return Err(ActorFatality::Internal);
        };
        if active_turn.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let candidate = InteractionRequestCandidate::new(
            completion.request_id,
            completion.item_id,
            completion.request,
        );
        let fact = lock(&conversation.live_state)
            .apply_interaction_request(candidate, completion.turn_id, completion.timestamp)
            .map_err(|_| ActorFatality::Integrity)?;
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        self.pending_interactions.insert(
            completion.request_id,
            ActiveInteraction {
                turn_id: completion.turn_id,
                item_id: completion.item_id,
                resolution_sender: completion.resolution_sender,
            },
        );
        self.publish_pending_interactions()?;
        if self
            .active_turn
            .as_ref()
            .is_some_and(|active_turn| active_turn.cancellation.is_cancelled())
        {
            let reason = self
                .emergency
                .observe(EmergencyControlTarget::Turn(completion.turn_id))
                .and_then(|observation| observation.signal())
                .map(|signal| match signal {
                    EmergencyControlSignal::Cancel => InteractionCancelReason::TurnCancelled,
                    EmergencyControlSignal::SecurityRevoked => {
                        InteractionCancelReason::SecurityRevoked
                    }
                    EmergencyControlSignal::PrepareForUnload => {
                        InteractionCancelReason::SessionUnloaded
                    }
                })
                .unwrap_or(InteractionCancelReason::TurnCancelled);
            self.cancel_pending_interaction(completion.request_id, completion.timestamp, reason)
                .await?;
        }
        Ok(())
    }

    async fn resolve_interaction_request(
        &mut self,
        request: &mut ResolveInteractionRequest,
    ) -> Result<(), ActorFatality> {
        let Some(resolution) = request.resolution.take() else {
            return Err(ActorFatality::Integrity);
        };
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let apply = lock(&conversation.live_state).apply_host_interaction_resolution(
            request.request_id,
            request.expected_turn_id,
            request.item_id,
            request.resolution_key.clone(),
            resolution,
            request.timestamp,
        );
        let (fact, resolution_for_worker) = match apply {
            Ok(HostInteractionResolutionApplyOutcome::Applied { fact, resolution }) => {
                (fact, resolution)
            }
            Ok(HostInteractionResolutionApplyOutcome::Idempotent) => {
                request.settle(Ok(()));
                return Ok(());
            }
            Err(error) => {
                request.settle(Err(match error {
                    HostInteractionResolutionError::NotFound => SessionInteractionError::NotFound,
                    HostInteractionResolutionError::ExpectedTurnMismatch => {
                        SessionInteractionError::ExpectedTurnMismatch
                    }
                    HostInteractionResolutionError::FamilyMismatch => {
                        SessionInteractionError::FamilyMismatch
                    }
                    HostInteractionResolutionError::InvalidResolution => {
                        SessionInteractionError::InvalidResolution
                    }
                    HostInteractionResolutionError::AlreadyResolved => {
                        SessionInteractionError::AlreadyResolved
                    }
                    HostInteractionResolutionError::CommandConflict => {
                        SessionInteractionError::CommandConflict
                    }
                    HostInteractionResolutionError::Internal => {
                        SessionInteractionError::InternalDispatchUnavailable
                    }
                }));
                return Ok(());
            }
        };
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        let active = self
            .pending_interactions
            .remove(&request.request_id)
            .ok_or(ActorFatality::Internal)?;
        if active.turn_id != request.expected_turn_id || active.item_id != request.item_id {
            return Err(ActorFatality::Integrity);
        }
        self.publish_pending_interactions()?;
        let _ = active.resolution_sender.send(resolution_for_worker);
        request.settle(Ok(()));
        Ok(())
    }

    async fn cancel_request(&mut self, request: &mut CancelRequest) -> Result<(), ActorFatality> {
        match request.target {
            SessionCancelTarget::Submit(command_id) => {
                let Some(active) = self.active_admission.as_ref() else {
                    if let Some(active) = self.active_turn.as_ref() {
                        if active.command_id == command_id {
                            if let Some(accepted) = active.cancel_accepted {
                                request.settle(if accepted.target == request.target {
                                    Ok(accepted)
                                } else {
                                    Err(SessionCancelError::TurnCancelling)
                                });
                                return Ok(());
                            }
                        }
                    }
                    request.settle(Err(SessionCancelError::SubmitNotCancellable));
                    return Ok(());
                };
                if active.command_id != command_id {
                    request.settle(Err(SessionCancelError::SubmitNotCancellable));
                    return Ok(());
                }
                if let Some(accepted) = active.cancel_accepted {
                    request.settle(if accepted.target == request.target {
                        Ok(accepted)
                    } else {
                        Err(SessionCancelError::TurnCancelling)
                    });
                    return Ok(());
                }
                let cancellation = active.cancellation.clone();
                let signal = self.signal_user_cancel(EmergencyControlTarget::Submit(command_id))?;
                let cancel_epoch = match signal {
                    EmergencyControlSignalOutcome::Accepted { epoch } => epoch,
                    EmergencyControlSignalOutcome::AlreadySignaled { .. } => {
                        request.settle(Err(SessionCancelError::TurnCancelling));
                        return Ok(());
                    }
                    EmergencyControlSignalOutcome::StaleTarget => {
                        return Err(ActorFatality::Integrity);
                    }
                };
                let accepted = SessionCancelAccepted {
                    target: request.target,
                    cancel_epoch,
                };
                self.active_admission
                    .as_mut()
                    .expect("the active admission remains installed")
                    .cancel_accepted = Some(accepted);
                if matches!(signal, EmergencyControlSignalOutcome::Accepted { .. }) {
                    cancellation.cancel();
                    self.execution_state = SessionExecutionState::Finishing;
                    self.publish_execution_state(
                        SessionExecutionState::Finishing,
                        None,
                        self.current.last_terminal(),
                    );
                }
                request.settle(Ok(accepted));
            }
            SessionCancelTarget::Turn(turn_id) => {
                if let Some(active) = self.active_admission.as_ref() {
                    if active.turn_id != turn_id {
                        request.settle(Err(SessionCancelError::ExpectedTurnMismatch));
                        return Ok(());
                    }
                    if let Some(accepted) = active.cancel_accepted {
                        request.settle(if accepted.target == request.target {
                            Ok(accepted)
                        } else {
                            Err(SessionCancelError::TurnCancelling)
                        });
                        return Ok(());
                    }
                    let emergency_target = active.emergency.target();
                    let cancellation = active.cancellation.clone();
                    let signal = self.signal_user_cancel(emergency_target)?;
                    let cancel_epoch = match signal {
                        EmergencyControlSignalOutcome::Accepted { epoch } => epoch,
                        EmergencyControlSignalOutcome::AlreadySignaled { .. } => {
                            request.settle(Err(SessionCancelError::TurnCancelling));
                            return Ok(());
                        }
                        EmergencyControlSignalOutcome::StaleTarget => {
                            return Err(ActorFatality::Integrity);
                        }
                    };
                    let accepted = SessionCancelAccepted {
                        target: request.target,
                        cancel_epoch,
                    };
                    self.active_admission
                        .as_mut()
                        .expect("the active admission remains installed")
                        .cancel_accepted = Some(accepted);
                    if matches!(signal, EmergencyControlSignalOutcome::Accepted { .. }) {
                        cancellation.cancel();
                        self.execution_state = SessionExecutionState::Finishing;
                        self.publish_execution_state(
                            SessionExecutionState::Finishing,
                            None,
                            self.current.last_terminal(),
                        );
                    }
                    request.settle(Ok(accepted));
                    return Ok(());
                }

                let Some(active) = self.active_turn.as_ref() else {
                    let error = if self
                        .current
                        .last_terminal()
                        .is_some_and(|(terminal_turn, _)| terminal_turn == turn_id)
                    {
                        SessionCancelError::TurnTerminal
                    } else {
                        SessionCancelError::TurnNotRunning
                    };
                    request.settle(Err(error));
                    return Ok(());
                };
                if active.turn_id != turn_id {
                    request.settle(Err(SessionCancelError::ExpectedTurnMismatch));
                    return Ok(());
                }
                if let Some(accepted) = active.cancel_accepted {
                    request.settle(if accepted.target == request.target {
                        Ok(accepted)
                    } else {
                        Err(SessionCancelError::TurnCancelling)
                    });
                    return Ok(());
                }
                let emergency_target = active.emergency.target();
                let cancellation = active.cancellation.clone();
                let signal = self.signal_user_cancel(emergency_target)?;
                let cancel_epoch = match signal {
                    EmergencyControlSignalOutcome::Accepted { epoch } => epoch,
                    EmergencyControlSignalOutcome::AlreadySignaled { .. } => {
                        request.settle(Err(SessionCancelError::TurnCancelling));
                        return Ok(());
                    }
                    EmergencyControlSignalOutcome::StaleTarget => {
                        return Err(ActorFatality::Integrity);
                    }
                };
                let accepted = SessionCancelAccepted {
                    target: request.target,
                    cancel_epoch,
                };
                self.active_turn
                    .as_mut()
                    .expect("the active turn remains installed")
                    .cancel_accepted = Some(accepted);
                if matches!(signal, EmergencyControlSignalOutcome::Accepted { .. }) {
                    cancellation.cancel();
                    self.execution_state = SessionExecutionState::Finishing;
                    self.steer.clear_for_turn(turn_id);
                    self.publish_execution_state(
                        SessionExecutionState::Finishing,
                        Some(turn_id),
                        self.current.last_terminal(),
                    );
                }
                let pending = self
                    .pending_interactions
                    .iter()
                    .filter_map(|(request_id, interaction)| {
                        (interaction.turn_id == turn_id).then_some(*request_id)
                    })
                    .collect::<Vec<_>>();
                request.settle(Ok(accepted));
                if matches!(signal, EmergencyControlSignalOutcome::Accepted { .. }) {
                    for request_id in pending {
                        self.cancel_pending_interaction(
                            request_id,
                            request.timestamp,
                            InteractionCancelReason::TurnCancelled,
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn security_revoke_request(
        &mut self,
        request: &mut SecurityRevokedRequest,
    ) -> Result<(), ActorFatality> {
        let (emergency_target, cancellation, security_revocation) = match request.target {
            SessionCancelTarget::Submit(command_id) => {
                let Some(active) = self.active_admission.as_ref() else {
                    request.settle(Err(SessionSecurityRevokedError::NotRunning));
                    return Ok(());
                };
                if active.command_id != command_id {
                    request.settle(Err(SessionSecurityRevokedError::NotRunning));
                    return Ok(());
                }
                (
                    EmergencyControlTarget::Submit(command_id),
                    active.cancellation.clone(),
                    Some(active.security_revocation.clone()),
                )
            }
            SessionCancelTarget::Turn(turn_id) => {
                if let Some(active) = self.active_admission.as_ref() {
                    if active.turn_id != turn_id {
                        request.settle(Err(SessionSecurityRevokedError::ExpectedTurnMismatch));
                        return Ok(());
                    }
                    (
                        active.emergency.target(),
                        active.cancellation.clone(),
                        Some(active.security_revocation.clone()),
                    )
                } else {
                    let Some(active) = self.active_turn.as_ref() else {
                        request.settle(Err(SessionSecurityRevokedError::NotRunning));
                        return Ok(());
                    };
                    if active.turn_id != turn_id {
                        request.settle(Err(SessionSecurityRevokedError::ExpectedTurnMismatch));
                        return Ok(());
                    }
                    (active.emergency.target(), active.cancellation.clone(), None)
                }
            }
        };

        let accepted = match self
            .emergency
            .signal(emergency_target, EmergencyControlSignal::SecurityRevoked)
        {
            EmergencyControlSignalOutcome::Accepted { .. } => {
                if let Some(security_revocation) = security_revocation {
                    security_revocation.cancel();
                }
                cancellation.cancel();
                true
            }
            EmergencyControlSignalOutcome::AlreadySignaled {
                signal: EmergencyControlSignal::Cancel,
                ..
            } => {
                request.settle(Err(SessionSecurityRevokedError::AlreadyCancelling));
                false
            }
            EmergencyControlSignalOutcome::AlreadySignaled {
                signal: EmergencyControlSignal::SecurityRevoked,
                ..
            } => {
                request.settle(Err(SessionSecurityRevokedError::AlreadyRevoked));
                false
            }
            EmergencyControlSignalOutcome::AlreadySignaled {
                signal: EmergencyControlSignal::PrepareForUnload,
                ..
            } => {
                // The unload already won the target; the Turn is being cancelled by unload.
                request.settle(Err(SessionSecurityRevokedError::AlreadyCancelling));
                false
            }
            EmergencyControlSignalOutcome::StaleTarget => return Err(ActorFatality::Integrity),
        };
        if accepted {
            let finishing_turn = match emergency_target {
                EmergencyControlTarget::Turn(turn_id) => Some(turn_id),
                EmergencyControlTarget::Submit(_) => {
                    self.active_admission.as_ref().and_then(|active| {
                        self.conversation.as_ref().and_then(|conversation| {
                            (lock(&conversation.live_state).current_turn() == Some(active.turn_id))
                                .then_some(active.turn_id)
                        })
                    })
                }
            };
            let pending = if let Some(turn_id) = finishing_turn {
                self.execution_state = SessionExecutionState::Finishing;
                self.publish_execution_state(
                    SessionExecutionState::Finishing,
                    Some(turn_id),
                    self.current.last_terminal(),
                );
                self.pending_interactions
                    .iter()
                    .filter_map(|(request_id, interaction)| {
                        (interaction.turn_id == turn_id).then_some(*request_id)
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            request.settle(Ok(()));
            for request_id in pending {
                self.cancel_pending_interaction(
                    request_id,
                    SystemClock.now(),
                    InteractionCancelReason::SecurityRevoked,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Accepts one host security Workspace invalidation.  The admission gate is already closed
    /// synchronously by the caller; the request arrives over the unbounded emergency lane so it
    /// can never be blocked behind the bounded work lane.  A duplicate invalidation joins the
    /// same state (no re-signal, no re-recovery, no CommandId).  If graceful-Unload preparation
    /// already started or the executor is closing, the request settles Closing.  Otherwise the
    /// actor registers the state and either: (a) an active admission/Turn exists — the exact
    /// current emergency target is signaled `SecurityRevoked` first-wins (an active
    /// definition/Agent/reload publication never shields or blocks this signal), the
    /// security_revocation/cancellation tokens are cancelled, and recovery starts after the
    /// admission failure cleanup or Turn terminal; (b) only an active publication remains —
    /// the publication is never blocked or cancelled, but the Idle Session still enters
    /// Preparing immediately (dropping the old WorkspaceSnapshot and publishing the single
    /// start `ReadinessChanged(command_id: None)`); the recovery worker itself starts with the
    /// post-publication exact current definition at the publication settlement; or (c) the
    /// Session is Idle — the actor enters Preparing immediately, publishes
    /// `ReadinessChanged(command_id: None)`, and starts the owner-tracked recovery worker.
    /// Waiters settle only after the recovery final state is installed.
    async fn begin_security_invalidation(
        &mut self,
        request: &mut SecurityInvalidationRequest,
    ) -> Result<(), ActorFatality> {
        let Some(response) = request.response.take() else {
            return Err(ActorFatality::Integrity);
        };
        if let Some(state) = self.security_invalidation.as_mut() {
            // A duplicate invalidation joins the same state; the recovery already owns the
            // signal and the worker, so nothing is re-signaled or re-run.
            state.waiters.push(response);
            return Ok(());
        }
        if self.closing.is_cancelled() || self.prepare_unload_accepted {
            let _ = response.send(Err(SessionSecurityInvalidationError::Closing));
            return Ok(());
        }
        let timestamp = request.timestamp;
        self.security_invalidation = Some(SecurityInvalidationState {
            timestamp,
            waiters: vec![response],
            worker_task: None,
        });
        // The exact emergency target is resolved from the active admission/Turn first: an
        // active publication never shields or blocks the security signal, so an admission/Turn
        // concurrent with a publication is still signaled SecurityRevoked now.  Only the
        // recovery itself waits for the publication settlement (which re-invokes
        // `start_security_recovery_worker` with the post-publication exact current definition).
        let (emergency_target, cancellation, security_revocation) =
            if let Some(active) = self.active_admission.as_ref() {
                (
                    active.emergency.target(),
                    active.cancellation.clone(),
                    Some(active.security_revocation.clone()),
                )
            } else if let Some(active) = self.active_turn.as_ref() {
                (active.emergency.target(), active.cancellation.clone(), None)
            } else if self.active_publication.is_some() {
                // No active admission/Turn, only an active publication: the publication is
                // never blocked or cancelled (it may already have crossed the durable
                // barrier), but the Session is Idle and the host restriction is already
                // current, so Preparing is entered immediately through the shared
                // `start_security_recovery_worker` order (the old WorkspaceSnapshot is
                // dropped and the single start `ReadinessChanged(command_id: None)` is
                // published); the worker itself still waits for the publication settlement
                // and re-resolves the post-publication exact current definition.
                self.start_security_recovery_worker()?;
                return Ok(());
            } else {
                // Idle with no active admission/Turn/publication: recovery starts immediately
                // (Preparing + worker are entered by `start_security_recovery_worker`).
                self.start_security_recovery_worker()?;
                return Ok(());
            };
        if self.signal_security_revocation(emergency_target)? {
            cancellation.cancel();
            if let Some(security_revocation) = security_revocation {
                security_revocation.cancel();
            }
            // Only a formed Turn projects Finishing; a pre-Input admission keeps Starting
            // legal and leaves the truth to the admission completion.
            let finishing_turn = match emergency_target {
                EmergencyControlTarget::Turn(turn_id) => Some(turn_id),
                EmergencyControlTarget::Submit(_) => {
                    self.active_admission.as_ref().and_then(|active| {
                        self.conversation.as_ref().and_then(|conversation| {
                            (lock(&conversation.live_state).current_turn()
                                == Some(active.turn_id))
                                .then_some(active.turn_id)
                        })
                    })
                }
            };
            if let Some(turn_id) = finishing_turn {
                if self.execution_state != SessionExecutionState::Finishing {
                    self.execution_state = SessionExecutionState::Finishing;
                    self.publish_execution_state(
                        SessionExecutionState::Finishing,
                        Some(turn_id),
                        self.current.last_terminal(),
                    );
                }
                let pending = self
                    .pending_interactions
                    .iter()
                    .filter_map(|(request_id, interaction)| {
                        (interaction.turn_id == turn_id).then_some(*request_id)
                    })
                    .collect::<Vec<_>>();
                for request_id in pending {
                    self.cancel_pending_interaction(
                        request_id,
                        SystemClock.now(),
                        InteractionCancelReason::SecurityRevoked,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// Signals `SecurityRevoked` first-wins on the exact target.  Returns whether security won
    /// the target; an earlier Cancel/SecurityRevoked/PrepareForUnload keeps its original signal
    /// and reason.
    fn signal_security_revocation(
        &self,
        target: EmergencyControlTarget,
    ) -> Result<bool, ActorFatality> {
        match self
            .emergency
            .signal(target, EmergencyControlSignal::SecurityRevoked)
        {
            EmergencyControlSignalOutcome::Accepted { .. } => Ok(true),
            EmergencyControlSignalOutcome::AlreadySignaled { .. } => Ok(false),
            EmergencyControlSignalOutcome::StaleTarget => Err(ActorFatality::Integrity),
        }
    }

    /// Enters the security-invalidation Preparing snapshot.  The Session must be Idle with no
    /// active admission or Turn; an in-flight publication is allowed because the host
    /// restriction is already current and the old WorkspaceSnapshot must be dropped even while
    /// the publication is still settling.  The old WorkspaceSnapshot is dropped and the
    /// workspace cause is masked; the public queue projection is re-run so the Preparing
    /// snapshot is legal: empty queues and accepting false.  The call is idempotent: it always
    /// re-applies `with_workspace_preparing` so `workspace` stays None (a settled publication
    /// cannot have reinstalled one while Preparing), but the single start
    /// `ReadinessChanged(command_id: None)` event is published only on the transition into
    /// Preparing — a re-entry after the publication settlement must not publish a second start
    /// event.
    fn enter_workspace_preparing(&mut self) -> Result<(), ActorFatality> {
        if !self.execution_state.is_idle()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
        {
            return Err(ActorFatality::Integrity);
        }
        let timestamp = self
            .security_invalidation
            .as_ref()
            .map(|state| state.timestamp)
            .ok_or(ActorFatality::Integrity)?;
        let was_preparing = self.current.workspace_preparing();
        self.publish_current(Arc::new(self.current.with_workspace_preparing()));
        self.publish_queue_projection();
        if !was_preparing {
            let _ = self
                .events
                .send(Arc::new(SessionExecutorEvent::ReadinessChanged {
                    timestamp,
                    command_id: None,
                    snapshot: Arc::clone(&self.current),
                }));
        }
        Ok(())
    }

    /// Starts the owner-tracked security Workspace recovery worker at the first legal Idle
    /// recovery start point.  It is a no-op when no invalidation is registered or when the
    /// worker already runs.  Preparing is entered as soon as the Session is Idle with no active
    /// admission/Turn — an in-flight publication does not block Preparing — and the worker
    /// spawns only once the publication also settles (the admission failure cleanup, Turn
    /// terminal, or publication settlement calls it again).  A closing executor settles the
    /// waiters with Closing and clears the state; a rejected spawn drops the never-polled
    /// future and its pre-constructed RAII guard reports the one typed fallback completion —
    /// Closing when the shared task owner is already closing, otherwise Internal plus closing
    /// both owners — without creating a second completion.
    fn start_security_recovery_worker(&mut self) -> Result<(), ActorFatality> {
        let Some(state) = self.security_invalidation.as_mut() else {
            return Ok(());
        };
        if state.worker_task.is_some() {
            return Ok(());
        }
        if self.closing.is_cancelled() {
            let state = self
                .security_invalidation
                .take()
                .expect("the registered invalidation state exists");
            for waiter in state.waiters {
                let _ = waiter.send(Err(SessionSecurityInvalidationError::Closing));
            }
            return Ok(());
        }
        if !self.execution_state.is_idle()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
        {
            // Not Idle yet: the caller re-invokes this after the admission failure cleanup or
            // the Turn terminal completes.
            return Ok(());
        }
        // The Session is Idle with no active admission/Turn: enter Preparing (drops the old
        // WorkspaceSnapshot, masks the workspace cause) before spawning the owner-tracked
        // recovery worker.  An in-flight publication does not block Preparing — the host
        // restriction is already current and the old WorkspaceSnapshot must be dropped even
        // while the publication is still settling.  When the invalidation was registered
        // during an active publication, Preparing was already entered at registration, so this
        // re-entry is idempotent: it re-asserts `workspace: None` against the settled current
        // snapshot (the publication completion cannot reinstall a WorkspaceSnapshot while
        // Preparing) and does not publish a second start `ReadinessChanged` event.  The state
        // timestamp is copied first so the `&mut self` call below is not aliased.
        let timestamp = state.timestamp;
        self.enter_workspace_preparing()?;
        if self.active_publication.is_some() {
            // The publication is still settling: the worker spawns only after the settlement,
            // which re-invokes this with the post-publication exact current definition.
            return Ok(());
        }
        let definition = Arc::clone(self.current.definition());
        let context = WorkspacePublicationContext {
            durable_state: self.durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            executor_closing: self.closing.clone(),
            candidate_cancellation: CancellationToken::new(),
        };
        #[cfg(test)]
        let hooks = Arc::clone(&self.hooks);
        // The RAII guard is created before the future is constructed.  If `spawn_tracked`
        // rejects the worker (owner closing, unavailable, or a spawn panic), the never-polled
        // future is dropped with the guard inside it, and the guard's Drop reports the one
        // fallback completion through the single settlement path — so the invalidation waiters
        // and the close drain can never wait for a completion that will not arrive.
        let guard = SecurityRecoveryCompletionGuard::new(
            self.completion_sender.clone(),
            timestamp,
            Arc::clone(&definition),
            self.task_context.clone(),
            self.durable_state.clone(),
        );
        let worker = async move {
            // The RAII guard also reports one fallback completion if the worker unwinds
            // mid-run, so the drain loop can never wait forever for a completion that will
            // not arrive.
            let mut guard = guard;
            #[cfg(test)]
            let result =
                run_security_workspace_recovery(context, definition, hooks).await;
            #[cfg(not(test))]
            let result = run_security_workspace_recovery(context, definition).await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.security_invalidation
                    .as_mut()
                    .expect("the invalidation state remains installed")
                    .worker_task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // The dropped worker future carries the RAII guard, whose Drop reports the one
                // typed Closing completion through the single settlement path; the actor or the
                // close drain settles the waiters exactly once.  Do not settle here or create a
                // second completion.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The dropped worker future carries the RAII guard, whose Drop reports the one
                // InternalDispatchUnavailable completion and closes both owners through the
                // single settlement path.  Do not settle here or create a second completion.
            }
        }
        Ok(())
    }

    /// Handles one security Workspace recovery worker completion.  The exact worker task is
    /// reaped first; a failed task, a fatal state, or an impossible shape settles every waiter
    /// exactly once with Internal.  An executor that is closing settles every waiter with
    /// Closing.  Otherwise the recovery final state is installed — the exact re-resolved
    /// WorkspaceSnapshot (verified by Arc identity and SessionId/revision) or the explicit new
    /// Workspace/Prompt Unavailable cause — one final `ReadinessChanged(command_id: None)` is
    /// published, every waiter settles Ok, and the admission gate reopens only while the
    /// executor is not closing and no graceful-Unload preparation was accepted (the sticky
    /// marker, which survives the Idle settlement).  A queued FollowUp may then hand off under
    /// the normal Ready rules.
    async fn handle_security_recovery_completion(
        &mut self,
        completion: SecurityRecoveryCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut state) = self.security_invalidation.take() else {
            // A spawn-failed worker's RAII guard can deliver its fallback completion after the
            // close path already settled the state; this is a benign close race, not corruption.
            return Ok(());
        };
        let worker_result = match state.worker_task.take() {
            Some(worker_task) => worker_task.wait().await,
            None => Ok(()),
        };
        if worker_result.is_err() || self.failure_state.is_fatal() {
            for waiter in state.waiters {
                let _ = waiter.send(Err(
                    SessionSecurityInvalidationError::InternalDispatchUnavailable,
                ));
            }
            if !self.failure_state.is_fatal() {
                self.close_for_fatal(ActorFatality::Internal);
            }
            return Err(ActorFatality::Internal);
        }
        if self.closing.is_cancelled() {
            for waiter in state.waiters {
                let _ = waiter.send(Err(SessionSecurityInvalidationError::Closing));
            }
            return Ok(());
        }
        match completion.result {
            SecurityRecoveryResult::Snapshot(snapshot) => {
                // The recovery must have resolved the exact definition it was spawned with: a
                // definition publication can never interleave (publications are rejected while
                // an invalidation is registered), so a mismatch is an integrity failure.
                if !Arc::ptr_eq(self.current.definition(), &completion.definition)
                    || snapshot.session_id() != completion.definition.session_id()
                    || snapshot.revision() != completion.definition.workspace().revision()
                {
                    for waiter in state.waiters {
                        let _ = waiter.send(Err(
                            SessionSecurityInvalidationError::InternalDispatchUnavailable,
                        ));
                    }
                    self.close_for_fatal(ActorFatality::Integrity);
                    return Err(ActorFatality::Integrity);
                }
                self.publish_current(Arc::new(
                    self.current.with_workspace_preparing_finished_success(snapshot),
                ));
            }
            SecurityRecoveryResult::Unavailable(cause) => {
                self.publish_current(Arc::new(
                    self.current.with_workspace_preparing_finished_failure(cause),
                ));
            }
            SecurityRecoveryResult::Closing => {
                // Cancellation through the executor token is caught by the check above.  A
                // resolver/task owner may also report Closing first; in that case transition
                // this executor into closing as ordinary ReloadWorkspace does, rather than
                // leaving a loaded Session permanently Preparing with its admission gate shut.
                for waiter in state.waiters {
                    let _ = waiter.send(Err(SessionSecurityInvalidationError::Closing));
                }
                self.turn_admission_gate.close();
                self.closing.cancel();
                return Ok(());
            }
            SecurityRecoveryResult::Internal => {
                for waiter in state.waiters {
                    let _ = waiter.send(Err(
                        SessionSecurityInvalidationError::InternalDispatchUnavailable,
                    ));
                }
                self.close_for_fatal(ActorFatality::Internal);
                return Err(ActorFatality::Internal);
            }
        }
        self.publish_queue_projection();
        // The final derived readiness can differ from Preparing for an Agent/model/prompt fact
        // (for example AgentUnavailable), so this final event is always published.
        let _ = self
            .events
            .send(Arc::new(SessionExecutorEvent::ReadinessChanged {
                timestamp: state.timestamp,
                command_id: None,
                snapshot: Arc::clone(&self.current),
            }));
        for waiter in state.waiters {
            let _ = waiter.send(Ok(()));
        }
        // Reopen only while the executor is not closing and no graceful-Unload preparation was
        // accepted; the sticky marker is never cleared, so a preparation that already settled
        // its waiters (state taken) still keeps the gate closed until close.  An Unavailable
        // Session is still rejected early by the readiness check.
        if !self.prepare_unload_accepted {
            self.turn_admission_gate.open();
        }
        self.settle_prepare_unload_if_idle();
        self.start_queued_follow_up_after_publication()?;
        Ok(())
    }

    /// Whether an owner-tracked security recovery worker is still running (its completion has
    /// not been handled yet).
    fn security_recovery_is_active(&self) -> bool {
        self.security_invalidation
            .as_ref()
            .is_some_and(|state| state.worker_task.is_some())
    }

    fn signal_emergency(
        &self,
        target: EmergencyControlTarget,
        signal: EmergencyControlSignal,
    ) -> Result<(), ActorFatality> {
        match self.emergency.signal(target, signal) {
            EmergencyControlSignalOutcome::Accepted { .. }
            | EmergencyControlSignalOutcome::AlreadySignaled { .. } => Ok(()),
            EmergencyControlSignalOutcome::StaleTarget => Err(ActorFatality::Integrity),
        }
    }

    fn signal_user_cancel(
        &self,
        target: EmergencyControlTarget,
    ) -> Result<EmergencyControlSignalOutcome, ActorFatality> {
        match self
            .emergency
            .signal(target, EmergencyControlSignal::Cancel)
        {
            outcome @ EmergencyControlSignalOutcome::Accepted { .. }
            | outcome @ EmergencyControlSignalOutcome::AlreadySignaled { .. } => Ok(outcome),
            EmergencyControlSignalOutcome::StaleTarget => Err(ActorFatality::Integrity),
        }
    }

    fn signal_emergency_cancel(&self, target: EmergencyControlTarget) -> Result<(), ActorFatality> {
        self.signal_emergency(target, EmergencyControlSignal::Cancel)
    }

    async fn cancel_pending_interaction(
        &mut self,
        request_id: RequestId,
        timestamp: Timestamp,
        reason: InteractionCancelReason,
    ) -> Result<(), ActorFatality> {
        let Some(active) = self.pending_interactions.get(&request_id) else {
            return Ok(());
        };
        let turn_id = active.turn_id;
        let candidate = InteractionResolutionCandidate::owner_cancellation(request_id, reason)
            .map_err(|_| ActorFatality::Integrity)?;
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let fact = match lock(&conversation.live_state)
            .apply_interaction_resolution(candidate, timestamp)
            .map_err(|_| ActorFatality::Integrity)?
        {
            InteractionResolutionApplyOutcome::Applied(fact) => fact,
            InteractionResolutionApplyOutcome::Idempotent { .. } => return Ok(()),
        };
        let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        let active = self
            .pending_interactions
            .remove(&request_id)
            .ok_or(ActorFatality::Internal)?;
        self.publish_pending_interactions()?;
        let _ = active
            .resolution_sender
            .send(ResolvedInteraction::cancelled_by_owner(reason).ok_or(ActorFatality::Integrity)?);
        debug_assert_eq!(active.turn_id, turn_id);
        Ok(())
    }

    fn publish_pending_interactions(&mut self) -> Result<(), ActorFatality> {
        let Some(conversation) = self.conversation.as_ref() else {
            return Err(ActorFatality::Integrity);
        };
        let pending = lock(&conversation.live_state).pending_interaction_facts();
        let (active_submit_command_id, follow_up_command_ids, steer_command_ids) =
            self.queue_projection(self.current.current_turn());
        let current = self
            .current
            .with_pending_interactions(pending)
            .with_queue_projection(
                active_submit_command_id,
                follow_up_command_ids,
                steer_command_ids,
            );
        self.publish_current(Arc::new(current));
        Ok(())
    }

    async fn handle_turn_completion(
        &mut self,
        completion: TurnCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_turn.take() else {
            return Err(ActorFatality::Internal);
        };
        let task_result = match active.task.take() {
            Some(task) => task.wait().await,
            None => Ok(()),
        };
        if active.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let Some(conversation) = self.conversation.as_ref().cloned() else {
            return Err(ActorFatality::Integrity);
        };
        conversation.clear_control_generation(active.turn_id, &active.control_generation);
        self.emergency.retire(active.emergency);
        let pending_before = self.pending_interactions.len();
        self.pending_interactions
            .retain(|_, interaction| interaction.turn_id != active.turn_id);
        if self.pending_interactions.len() != pending_before {
            self.publish_pending_interactions()?;
        }
        self.steer.clear_for_turn(active.turn_id);
        let live_turn = lock(&conversation.live_state).current_turn();
        if live_turn == Some(active.turn_id) {
            lock(&conversation.live_state)
                .fail_current_turn(active.turn_id)
                .map_err(|_| ActorFatality::Integrity)?;
        } else if live_turn.is_some() {
            return Err(ActorFatality::Integrity);
        }
        if task_result.is_err() {
            self.task_context.request_closing();
            self.durable_state.request_closing();
        }
        self.execution_state = SessionExecutionState::Idle;
        self.publish_execution_state(
            SessionExecutionState::Idle,
            None,
            Some((active.turn_id, completion.terminal)),
        );
        // The Session is Idle again: apply the latest intended availability composite before
        // the FollowUp decision, so a Disable at terminal leaves the queue intact for a later
        // Enable and an Enable can hand off one FollowUp immediately, while a shared-resource
        // reload fact observed during the Turn applies here with the last command attribution.
        // A pending composite is never applied to a legal non-Idle snapshot.
        self.apply_pending_availability()?;
        // A FollowUp is only popped and started here when no publication is active, no
        // security Workspace recovery is pending, no graceful-Unload preparation has been
        // accepted (the sticky marker, which survives the Idle settlement), and the Session is
        // Ready.  If a future-only definition publication is still in flight at terminal, or
        // the Agent is unavailable, or the executor is preparing for unload, or a security
        // invalidation holds the Session, the FollowUp stays queued: the publication
        // settlement, the recovery completion, or the next Enable starts it, so a queued
        // FollowUp is never dropped (the prepare path has already cleared the queues).
        let queued_follow_up = if task_result.is_ok()
            && !self.prepare_unload_accepted
            && self.security_invalidation.is_none()
            && self.active_publication.is_none()
            && matches!(self.current.readiness(), SessionReadinessView::Ready)
        {
            self.follow_up.pop_front()
        } else {
            None
        };
        if queued_follow_up.is_some() {
            // Re-project the queue so the terminal snapshot reflects the popped FollowUp.
            self.publish_queue_projection();
        }
        let _ = self
            .events
            .send(Arc::new(SessionExecutorEvent::TurnTerminal {
                timestamp: SystemClock.now(),
                command_id: active.command_id,
                turn_id: active.turn_id,
                terminal: completion.terminal,
                snapshot: Arc::clone(&self.current),
            }));
        // The Turn terminal is published first (TurnInterrupted with SecurityRevoked or the
        // earlier winner), then the security recovery enters Preparing and publishes
        // ReadinessChanged(command_id: None) before any FollowUp handoff.
        self.start_security_recovery_worker()?;
        if let Some(queued) = queued_follow_up {
            let (command_id, intent) = queued.into_parts();
            let mut request = SubmitRequest {
                command_id,
                intent: Some(intent),
                response: None,
            };
            self.start_admission(&mut request)?;
        }
        self.settle_prepare_unload_if_idle();
        if task_result.is_err() {
            Err(ActorFatality::Internal)
        } else {
            Ok(())
        }
    }

    fn start_publication(
        &mut self,
        request: &mut WorkspaceDefinitionRequest,
    ) -> Result<(), ActorFatality> {
        if self.active_publication.is_some() {
            request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
            return Ok(());
        }
        // While a security Workspace invalidation is registered (Preparing or recovery
        // pending), no new publication is admitted: the recovery worker resolved the exact
        // current definition and any interleaved definition install would break its Arc
        // identity.  The host seam itself never blocks an already-active publication.
        if self.security_invalidation.is_some() {
            request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
            return Ok(());
        }
        if self.current.definition().revision() != request.expected_revision {
            request.settle(Err(SessionWorkspaceDefinitionError::StaleRevision));
            return Ok(());
        }
        if self.task_context.is_closing() {
            request.settle(Err(SessionWorkspaceDefinitionError::Closing));
            return Ok(());
        }
        if request.candidate_cancellation.is_cancelled() {
            request.settle(Err(SessionWorkspaceDefinitionError::Closing));
            return Ok(());
        }

        let attempt = SealedSessionDefinitionAttempt::new(
            self.current.definition().session_id(),
            request.expected_revision,
            request.workspace.clone(),
            request.model.clone(),
            request.prompts.clone(),
            request.owner_timestamp,
        );
        // This is deliberately before publication admission and before any resolver call.  In
        // particular, stale wins over canonical no-op.
        let expected = match attempt.decide(SessionLifecycle::Open, self.current.definition()) {
            Ok(SessionDefinitionDecision::NoChange) => ExpectedPublication::NoChange {
                definition: Arc::clone(self.current.definition()),
            },
            Ok(SessionDefinitionDecision::Publish(definition)) => {
                let workspace_changed = definition.workspace().revision()
                    != self.current.definition().workspace().revision();
                // A true Workspace semantic change is only acceptable while Idle.  A
                // canonical-equivalent Workspace combined with a Model/Prompt change is
                // future-only and is allowed during active execution.
                if workspace_changed && !self.execution_state.is_idle() {
                    request.settle(Err(SessionWorkspaceDefinitionError::SessionBusy));
                    return Ok(());
                }
                ExpectedPublication::Publish {
                    definition: Arc::new(definition),
                    workspace_changed,
                }
            }
            Err(SessionDefinitionDecisionError::StaleRevision) => {
                request.settle(Err(SessionWorkspaceDefinitionError::StaleRevision));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::SessionArchived) => {
                request.settle(Err(SessionWorkspaceDefinitionError::SessionArchived));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::SessionDeleted) => {
                request.settle(Err(SessionWorkspaceDefinitionError::SessionDeleted));
                return Ok(());
            }
            Err(SessionDefinitionDecisionError::RevisionExhausted) => {
                request.settle(Err(SessionWorkspaceDefinitionError::StateTooLarge));
                return Ok(());
            }
        };

        let permit = SessionDefinitionPublicationPermit::new();
        let waiter = Arc::new(PublicationWaiterState::new(
            request
                .response
                .take()
                .expect("an admitted update request owns one waiter"),
        ));
        self.failure_state.install(Arc::clone(&waiter));
        let active = ActivePublication {
            permit: permit.clone(),
            expected: expected.clone(),
            timestamp: request.owner_timestamp,
            command_id: request.command_id,
            waiter,
            worker_task: None,
        };
        // Install the actor publication state before spawning any asynchronous work.  The second
        // request is therefore Busy even if the owner scheduler immediately runs the worker.
        self.active_publication = Some(active);

        let completion_sender = self.completion_sender.clone();
        let task_context = self.task_context.clone();
        let durable_state = self.durable_state.clone();
        let publication_context = WorkspacePublicationContext {
            durable_state: durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            executor_closing: self.closing.clone(),
            candidate_cancellation: request.candidate_cancellation.clone(),
        };
        let session_id = self.current.definition().session_id();
        let expected_for_worker = expected.clone();
        #[cfg(test)]
        let hooks_for_worker = Arc::clone(&self.hooks);
        let guard = PublicationCompletionGuard::new(
            completion_sender.clone(),
            permit,
            task_context.clone(),
            durable_state.clone(),
        );
        let worker = async move {
            let mut guard = guard;
            #[cfg(test)]
            let result = run_publication(
                publication_context,
                session_id,
                attempt,
                expected_for_worker,
                hooks_for_worker,
            )
            .await;
            #[cfg(not(test))]
            let result = run_publication(
                publication_context,
                session_id,
                attempt,
                expected_for_worker,
            )
            .await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_publication
                    .as_mut()
                    .expect("the active publication is installed before spawning")
                    .worker_task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // A closing owner cannot admit a worker.  The guard's completion is still the
                // single settlement path; its redacted Closing result is mapped by the actor.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The moved RAII guard reports the one InternalDispatchUnavailable completion and
                // closes both owners.  Do not settle here or create a second completion.
            }
        }
        Ok(())
    }

    fn start_agent_upgrade_publication(
        &mut self,
        request: &mut AgentUpgradeRequest,
    ) -> Result<(), ActorFatality> {
        if self.active_publication.is_some() {
            request.settle(Err(SessionDefinitionPublicationError::SessionBusy));
            return Ok(());
        }
        // See `start_publication`: no new publication while a security invalidation is
        // registered, so the recovery worker's exact definition Arc stays valid.
        if self.security_invalidation.is_some() {
            request.settle(Err(SessionDefinitionPublicationError::SessionBusy));
            return Ok(());
        }
        if self.current.definition().revision() != request.expected_revision {
            request.settle(Err(SessionDefinitionPublicationError::StaleRevision));
            return Ok(());
        }
        if self.task_context.is_closing() {
            request.settle(Err(SessionDefinitionPublicationError::Closing));
            return Ok(());
        }
        if request.candidate_cancellation.is_cancelled() {
            request.settle(Err(SessionDefinitionPublicationError::Closing));
            return Ok(());
        }

        let attempt = SealedSessionAgentUpgradeAttempt::new(
            self.current.definition().session_id(),
            request.expected_revision,
            request.target,
            request.owner_timestamp,
        );
        // The executor deliberately does not resolve target current, retained membership, Agent
        // status, or a candidate definition: those facts are resolved only inside DurableState
        // under its Agent → Session publication gates.  The durable outcome therefore decides
        // changed/no-op and the actor validates its exact shape before installation.
        let expected = ExpectedPublication::AgentUpgrade {
            expected_revision: request.expected_revision,
        };

        let permit = SessionDefinitionPublicationPermit::new();
        let waiter = Arc::new(PublicationWaiterState::new(
            request
                .response
                .take()
                .expect("an admitted Agent upgrade request owns one waiter"),
        ));
        self.failure_state.install(Arc::clone(&waiter));
        let active = ActivePublication {
            permit: permit.clone(),
            expected: expected.clone(),
            timestamp: request.owner_timestamp,
            command_id: request.command_id,
            waiter,
            worker_task: None,
        };
        // Install the actor publication state before spawning any asynchronous work.  The second
        // request is therefore Busy even if the owner scheduler immediately runs the worker.
        self.active_publication = Some(active);

        let completion_sender = self.completion_sender.clone();
        let task_context = self.task_context.clone();
        let durable_state = self.durable_state.clone();
        let publication_context = WorkspacePublicationContext {
            durable_state: durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            executor_closing: self.closing.clone(),
            candidate_cancellation: request.candidate_cancellation.clone(),
        };
        #[cfg(test)]
        let hooks_for_worker = Arc::clone(&self.hooks);
        let guard = PublicationCompletionGuard::new(
            completion_sender.clone(),
            permit,
            task_context.clone(),
            durable_state.clone(),
        );
        let worker = async move {
            let mut guard = guard;
            #[cfg(test)]
            let result =
                run_agent_upgrade_publication(publication_context, attempt, hooks_for_worker).await;
            #[cfg(not(test))]
            let result = run_agent_upgrade_publication(publication_context, attempt).await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_publication
                    .as_mut()
                    .expect("the active Agent upgrade publication is installed before spawning")
                    .worker_task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // A closing owner cannot admit a worker.  The guard's completion is still the
                // single settlement path; its redacted Closing result is mapped by the actor.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The moved RAII guard reports the one InternalDispatchUnavailable completion and
                // closes both owners.  Do not settle here or create a second completion.
            }
        }
        Ok(())
    }

    fn start_workspace_reload(
        &mut self,
        request: &mut ReloadWorkspaceRequest,
    ) -> Result<(), ActorFatality> {
        if self.active_publication.is_some() {
            request.settle(Err(SessionDefinitionPublicationError::SessionBusy));
            return Ok(());
        }
        // See `start_publication`: no new publication while a security invalidation is
        // registered, so the recovery worker's exact definition Arc stays valid.
        if self.security_invalidation.is_some() {
            request.settle(Err(SessionDefinitionPublicationError::SessionBusy));
            return Ok(());
        }
        if !self.execution_state.is_idle() {
            request.settle(Err(SessionDefinitionPublicationError::SessionBusy));
            return Ok(());
        }
        if self.task_context.is_closing() {
            request.settle(Err(SessionDefinitionPublicationError::Closing));
            return Ok(());
        }
        if request.candidate_cancellation.is_cancelled() {
            request.settle(Err(SessionDefinitionPublicationError::Closing));
            return Ok(());
        }

        // The reload never publishes a durable definition: the worker re-resolves the exact
        // currently installed definition Workspace, and the actor validates after completion
        // that this exact definition (and its Workspace revision) is still installed before
        // atomically replacing only the WorkspaceSnapshot Arc.
        let expected = ExpectedPublication::ReloadWorkspace {
            definition: Arc::clone(self.current.definition()),
        };

        let permit = SessionDefinitionPublicationPermit::new();
        let waiter = Arc::new(PublicationWaiterState::new(
            request
                .response
                .take()
                .expect("an admitted reload request owns one waiter"),
        ));
        self.failure_state.install(Arc::clone(&waiter));
        let active = ActivePublication {
            permit: permit.clone(),
            expected: expected.clone(),
            timestamp: request.owner_timestamp,
            command_id: request.command_id,
            waiter,
            worker_task: None,
        };
        // Install the actor publication state before spawning any asynchronous work.  The second
        // request is therefore Busy even if the owner scheduler immediately runs the worker.
        self.active_publication = Some(active);

        let completion_sender = self.completion_sender.clone();
        let task_context = self.task_context.clone();
        let durable_state = self.durable_state.clone();
        let publication_context = WorkspacePublicationContext {
            durable_state: durable_state.clone(),
            resolver: Arc::clone(&self.resolver),
            prompt_service: Arc::clone(&self.prompt_service),
            executor_closing: self.closing.clone(),
            candidate_cancellation: request.candidate_cancellation.clone(),
        };
        let definition_for_worker = Arc::clone(expected.definition());
        #[cfg(test)]
        let hooks_for_worker = Arc::clone(&self.hooks);
        let guard = PublicationCompletionGuard::new(
            completion_sender.clone(),
            permit,
            task_context.clone(),
            durable_state.clone(),
        );
        let worker = async move {
            let mut guard = guard;
            #[cfg(test)]
            let result =
                run_workspace_reload(publication_context, definition_for_worker, hooks_for_worker)
                    .await;
            #[cfg(not(test))]
            let result = run_workspace_reload(publication_context, definition_for_worker).await;
            guard.complete(result);
        };
        match self.task_context.spawn_tracked(worker) {
            Ok(task) => {
                self.active_publication
                    .as_mut()
                    .expect("the active reload publication is installed before spawning")
                    .worker_task = Some(task);
            }
            Err(RuntimeTaskError::OwnerClosing) => {
                // A closing owner cannot admit a worker.  The guard's completion is still the
                // single settlement path; its redacted Closing result is mapped by the actor.
            }
            Err(RuntimeTaskError::OperationPanicked | RuntimeTaskError::WorkerUnavailable) => {
                // The moved RAII guard reports the one InternalDispatchUnavailable completion and
                // closes both owners.  Do not settle here or create a second completion.
            }
        }
        Ok(())
    }

    async fn handle_completion(
        &mut self,
        completion: ExecutorCompletion,
    ) -> Result<(), ActorFatality> {
        match completion {
            ExecutorCompletion::Publication(completion) => {
                self.handle_publication_completion(completion).await
            }
            ExecutorCompletion::Admission(completion) => {
                self.handle_admission_completion(completion).await
            }
            ExecutorCompletion::InteractionRequested(completion) => {
                self.handle_interaction_requested(completion).await
            }
            ExecutorCompletion::SteerSafePoint(completion) => {
                self.handle_steer_safe_point(completion).await
            }
            ExecutorCompletion::TurnPhase(completion) => {
                self.handle_turn_phase_completion(completion)
            }
            ExecutorCompletion::Turn(completion) => self.handle_turn_completion(completion).await,
            ExecutorCompletion::SecurityRecovery(completion) => {
                self.handle_security_recovery_completion(completion).await
            }
        }
    }

    fn handle_turn_phase_completion(
        &mut self,
        completion: TurnPhaseCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(active_turn) = self.active_turn.as_mut() else {
            return Err(ActorFatality::Internal);
        };
        if active_turn.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        active_turn.phase = completion.phase;
        self.publish_queue_projection();
        let _ = completion.response.send(());
        Ok(())
    }

    async fn handle_steer_safe_point(
        &mut self,
        completion: SteerSafePointCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(active_turn) = self.active_turn.as_ref() else {
            return Err(ActorFatality::Internal);
        };
        if active_turn.turn_id != completion.turn_id {
            return Err(ActorFatality::Internal);
        }
        let steer = self.steer.pop_front_for_turn(completion.turn_id);
        if steer.is_some() || completion.refresh_public_snapshot {
            self.publish_queue_projection();
        }
        if steer.is_none() && completion.close_if_empty {
            if let Some(active_turn) = self.active_turn.as_mut() {
                active_turn.steer_admission_open = false;
            }
        }
        #[cfg(test)]
        self.hooks.after_steer_arbitration().await;
        let _ = completion.response.send(steer);
        Ok(())
    }

    /// Synchronous helper that classifies one Session definition's model availability against
    /// the installed Runtime-owned Turn catalog through the exact Turn model resolution seam.
    /// The test-only dependency shape without Turn resources captures no model fact and
    /// defaults to available; production always carries the Runtime-owned catalog, so a
    /// CatalogUnavailable/SourceUnavailable/InvalidDefinition result there is an internal
    /// invariant for the caller's fatal path.
    fn definition_model_available(
        &self,
        definition: Arc<SessionDefinition>,
    ) -> Result<bool, ModelResolutionError> {
        let Some(resources) = self.turn_resources.as_ref() else {
            return Ok(true);
        };
        model_available_for_definition(
            &resources.model_gateway,
            Arc::clone(&resources.model_catalog),
            &definition,
        )
    }

    /// Async helper that classifies one Session definition's selected-Prompt availability
    /// against the current installed Runtime-owned Prompt resources through the exact
    /// `for_turn` selection stage.  The test-only dependency shape without Turn resources
    /// captures no Prompt fact and defaults to available; production always carries the
    /// Runtime-owned Prompt view, so a Closing or any other failure there is an internal
    /// invariant for the caller's fatal path, never a fabricated PromptUnavailable.
    async fn definition_prompt_available(
        &self,
        definition: Arc<SessionDefinition>,
    ) -> Result<bool, SessionPromptAvailabilityError> {
        let Some(resources) = self.turn_resources.as_ref() else {
            return Ok(true);
        };
        prompt_available_for_definition(
            self.durable_state.clone(),
            Arc::clone(&self.prompt_service),
            Arc::clone(&resources.prompt_resources),
            &definition,
        )
        .await
    }

    async fn handle_publication_completion(
        &mut self,
        completion: PublicationCompletion,
    ) -> Result<(), ActorFatality> {
        let Some(mut active) = self.active_publication.take() else {
            self.task_context.request_closing();
            self.durable_state.request_closing();
            return Err(ActorFatality::Internal);
        };

        let PublicationCompletion {
            permit: completion_permit,
            result,
        } = completion;
        let permit_matches = active.permit.same_as(&completion_permit);
        let worker_result = match active.worker_task.take() {
            Some(worker_task) => worker_task.wait().await,
            None => Ok(()),
        };
        if worker_result.is_err() {
            self.active_publication = Some(active);
            self.close_for_fatal(ActorFatality::Internal);
            return Err(ActorFatality::Internal);
        }
        if !permit_matches {
            self.active_publication = Some(active);
            self.close_for_fatal(ActorFatality::Integrity);
            return Err(ActorFatality::Integrity);
        }

        let handling = self.validate_completion(&active.expected, active.timestamp, result);
        match handling {
            CompletionHandling::Success(outcome, new_snapshot, new_definition) => {
                if new_snapshot.is_some() || new_definition.is_some() {
                    if self.installation_fault_is_armed() {
                        self.active_publication = Some(active);
                        self.close_for_fatal(ActorFatality::Integrity);
                        return Err(ActorFatality::Integrity);
                    }
                    // Any installed new definition carries its own model and selected-Prompt
                    // availability facts, resolved against the current installed Runtime-owned
                    // catalog/Prompt resources before installation, so an ordinary future-only
                    // Model/Prompt replacement, true Workspace publication, or Agent revision
                    // upgrade can both restore and degrade readiness atomically with the
                    // definition.  A Workspace reload (new_definition None) preserves the exact
                    // installed definition Arc and keeps the current model and selected-Prompt
                    // availability facts.  A real Workspace installation clears only the
                    // workspace Unavailable cause; a disabled Agent keeps AgentUnavailable until
                    // it is re-enabled.  with_definition and with_definition_and_workspace keep
                    // every immutable observation fact (usage, recording, diagnostics, public
                    // pending interactions) in all cases, and the
                    // DefinitionUpdated/WorkspaceReloaded event snapshot carries the
                    // re-projected readiness without an extra ReadinessChanged event.
                    let prompt_available = match new_definition.as_ref() {
                        Some(definition) => {
                            match self.definition_prompt_available(Arc::clone(definition)).await {
                                Ok(available) => available,
                                Err(_) => {
                                    self.active_publication = Some(active);
                                    self.close_for_fatal(ActorFatality::Internal);
                                    return Err(ActorFatality::Internal);
                                }
                            }
                        }
                        None => self.current.prompt_available(),
                    };
                    let model_available = match new_definition.as_ref() {
                        Some(definition) => {
                            match self.definition_model_available(Arc::clone(definition)) {
                                Ok(available) => available,
                                Err(_) => {
                                    // A resolution failure other than the three ordinary model
                                    // incompatibilities on the installed Runtime-owned catalog is an
                                    // internal invariant, never a fabricated ModelUnavailable.
                                    self.active_publication = Some(active);
                                    self.close_for_fatal(ActorFatality::Internal);
                                    return Err(ActorFatality::Internal);
                                }
                            }
                        }
                        None => self.current.model_available(),
                    };
                    let current = match (new_snapshot, new_definition) {
                        (Some(snapshot), Some(definition)) => self.current
                            .with_definition_and_workspace(
                                definition,
                                snapshot,
                                model_available,
                                prompt_available,
                            ),
                        (Some(snapshot), None) => self.current.with_definition_and_workspace(
                            Arc::clone(self.current.definition()),
                            snapshot,
                            model_available,
                            prompt_available,
                        ),
                        (None, Some(definition)) => {
                            self.current
                                .with_definition(definition, model_available, prompt_available)
                        }
                        (None, None) => {
                            self.active_publication = Some(active);
                            self.close_for_fatal(ActorFatality::Integrity);
                            return Err(ActorFatality::Integrity);
                        }
                    };
                    let (active_submit_command_id, follow_up_command_ids, steer_command_ids) =
                        self.queue_projection(self.current.current_turn());
                    let current = Arc::new(
                        current.with_queue_projection(
                            active_submit_command_id,
                            follow_up_command_ids,
                            steer_command_ids,
                        ),
                    );
                    self.publish_current(current);
                    let _ = self.events.send(Arc::new(match outcome {
                        SessionWorkspaceDefinitionOutcome::Reloaded { .. } => {
                            SessionExecutorEvent::WorkspaceReloaded {
                                timestamp: active.timestamp,
                                command_id: active.command_id,
                                snapshot: Arc::clone(&self.current),
                            }
                        }
                        SessionWorkspaceDefinitionOutcome::NoChange { .. }
                        | SessionWorkspaceDefinitionOutcome::Updated { .. } => {
                            SessionExecutorEvent::DefinitionUpdated {
                                timestamp: active.timestamp,
                                command_id: active.command_id,
                                snapshot: Arc::clone(&self.current),
                            }
                        }
                    }));
                }
                active.waiter.settle(Ok(outcome));
                self.finish_active_waiter(&active.waiter);
                self.start_queued_follow_up_after_publication()?;
                self.settle_prepare_unload_if_idle();
                // A security invalidation registered while this publication was active starts
                // its Workspace recovery with the post-publication exact current definition.
                self.start_security_recovery_worker()?;
                Ok(())
            }
            CompletionHandling::Ordinary(error) => {
                if matches!(
                    error,
                    SessionWorkspaceDefinitionError::InternalDispatchUnavailable
                ) {
                    self.active_publication = Some(active);
                    self.close_for_fatal(ActorFatality::Internal);
                    return Err(ActorFatality::Internal);
                } else if matches!(error, SessionWorkspaceDefinitionError::Closing) {
                    self.turn_admission_gate.close();
                    self.closing.cancel();
                }
                active.waiter.settle(Err(error));
                self.finish_active_waiter(&active.waiter);
                if !matches!(error, SessionWorkspaceDefinitionError::Closing) {
                    self.start_queued_follow_up_after_publication()?;
                }
                self.settle_prepare_unload_if_idle();
                // A security invalidation registered while this publication was active starts
                // its Workspace recovery after the ordinary settlement (the current definition
                // is unchanged on an ordinary failure).
                self.start_security_recovery_worker()?;
                Ok(())
            }
            CompletionHandling::Fatal(fatality) => {
                self.active_publication = Some(active);
                self.close_for_fatal(fatality);
                Err(fatality)
            }
        }
    }

    /// Starts the next queued FollowUp after a publication settles, a security Workspace
    /// recovery completes, or an Enable restores Ready, when the Session is Idle, Ready, and no
    /// admission, Turn, or publication remains.  A FollowUp left queued at Turn terminal
    /// because a future-only publication was still active, a security invalidation was pending,
    /// or the Agent was unavailable is therefore never lost: it starts here against the
    /// post-publication current definition, the post-recovery readiness, or the next Enable.
    fn start_queued_follow_up_after_publication(&mut self) -> Result<(), ActorFatality> {
        if self.closing.is_cancelled()
            || self.prepare_unload_accepted
            || self.security_invalidation.is_some()
            || self.active_publication.is_some()
            || self.active_admission.is_some()
            || self.active_turn.is_some()
            || !self.execution_state.is_idle()
            || !matches!(self.current.readiness(), SessionReadinessView::Ready)
        {
            return Ok(());
        }
        if let Some(queued) = self.follow_up.pop_front() {
            let (command_id, intent) = queued.into_parts();
            let mut request = SubmitRequest {
                command_id,
                intent: Some(intent),
                response: None,
            };
            self.start_admission(&mut request)?;
        }
        Ok(())
    }

    fn validate_completion(
        &self,
        expected: &ExpectedPublication,
        owner_timestamp: Timestamp,
        result: PublicationCompletionResult,
    ) -> CompletionHandling {
        match result {
            PublicationCompletionResult::Error(error) => {
                if matches!(
                    error,
                    SessionDefinitionPublicationError::InternalDispatchUnavailable
                ) {
                    CompletionHandling::Fatal(ActorFatality::Internal)
                } else {
                    CompletionHandling::Ordinary(error)
                }
            }
            PublicationCompletionResult::AgentUpgrade { outcome } => {
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return CompletionHandling::Ordinary(map_durable_agent_upgrade_error(
                            error,
                        ));
                    }
                };
                match (expected, outcome) {
                    (
                        ExpectedPublication::AgentUpgrade { expected_revision },
                        DurableSessionAgentUpgradeOutcome::NoChange(head, returned),
                    ) => {
                        // A canonical no-op must return the exact installed head/definition and
                        // never install a snapshot or emit an event.
                        if self.current.definition().revision() != *expected_revision
                            || !valid_durable_definition_shape(
                                head.as_ref(),
                                returned.as_ref(),
                                self.current.definition().as_ref(),
                            )
                            || returned.as_ref() != self.current.definition().as_ref()
                        {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionDefinitionPublicationOutcome::NoChange {
                                    definition_revision: returned.revision(),
                                    workspace_revision: returned.workspace().revision(),
                                },
                                None,
                                None,
                            )
                        }
                    }
                    (
                        ExpectedPublication::AgentUpgrade { expected_revision },
                        DurableSessionAgentUpgradeOutcome::Updated(head, returned),
                    ) => {
                        // The durable candidate must be the exact checked successor pinning the
                        // same AgentId at a different retained revision with every other durable
                        // fact unchanged; no prebuilt WorkspaceSnapshot may exist.
                        if !valid_durable_agent_upgrade_shape(
                            head.as_ref(),
                            returned.as_ref(),
                            self.current.definition().as_ref(),
                            *expected_revision,
                            owner_timestamp,
                        ) {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionDefinitionPublicationOutcome::Updated {
                                    definition_revision: returned.revision(),
                                    workspace_revision: returned.workspace().revision(),
                                },
                                None,
                                Some(returned),
                            )
                        }
                    }
                    // A durable outcome with the wrong changed/no-op shape is an integrity
                    // failure.  It may already have crossed the Store commit point.
                    _ => CompletionHandling::Fatal(ActorFatality::Integrity),
                }
            }
            PublicationCompletionResult::Durable { outcome, snapshot } => {
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        return CompletionHandling::Ordinary(map_durable_definition_error(error));
                    }
                };
                match (expected, outcome) {
                    (
                        ExpectedPublication::NoChange { definition },
                        DurableSessionDefinitionOutcome::NoChange(head, returned),
                    ) => {
                        if snapshot.is_some()
                            || !valid_durable_definition_shape(
                                head.as_ref(),
                                returned.as_ref(),
                                definition.as_ref(),
                            )
                            || returned.as_ref() != definition.as_ref()
                        {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionWorkspaceDefinitionOutcome::NoChange {
                                    definition_revision: returned.revision(),
                                    workspace_revision: returned.workspace().revision(),
                                },
                                None,
                                None,
                            )
                        }
                    }
                    (
                        ExpectedPublication::Publish {
                            definition,
                            workspace_changed,
                        },
                        DurableSessionDefinitionOutcome::Updated(head, returned),
                    ) => {
                        if !valid_durable_definition_shape(
                            head.as_ref(),
                            returned.as_ref(),
                            definition.as_ref(),
                        ) || returned.as_ref() != definition.as_ref()
                        {
                            return CompletionHandling::Fatal(ActorFatality::Integrity);
                        }
                        if *workspace_changed {
                            let Some(snapshot) = snapshot else {
                                return CompletionHandling::Fatal(ActorFatality::Integrity);
                            };
                            if snapshot.session_id() != definition.session_id()
                                || snapshot.revision() != definition.workspace().revision()
                            {
                                CompletionHandling::Fatal(ActorFatality::Integrity)
                            } else {
                                CompletionHandling::Success(
                                    SessionWorkspaceDefinitionOutcome::Updated {
                                        definition_revision: returned.revision(),
                                        workspace_revision: returned.workspace().revision(),
                                    },
                                    Some(snapshot),
                                    Some(returned),
                                )
                            }
                        } else {
                            // Future-only replacement preserves the exact installed Workspace
                            // Snapshot (or its absence while Unavailable); the durable candidate
                            // must therefore keep the current WorkspaceRevision, and no prebuilt
                            // snapshot may exist.  The current WorkspaceRevision is read from the
                            // installed durable definition so the check also covers an
                            // Unavailable Session that has no WorkspaceSnapshot.
                            if snapshot.is_some()
                                || returned.workspace().revision()
                                    != self.current.workspace_revision()
                            {
                                CompletionHandling::Fatal(ActorFatality::Integrity)
                            } else {
                                CompletionHandling::Success(
                                    SessionWorkspaceDefinitionOutcome::Updated {
                                        definition_revision: returned.revision(),
                                        workspace_revision: returned.workspace().revision(),
                                    },
                                    None,
                                    Some(returned),
                                )
                            }
                        }
                    }
                    // A durable outcome with the wrong changed/no-op shape is an integrity
                    // failure.  It may already have crossed the Store commit point.
                    _ => CompletionHandling::Fatal(ActorFatality::Integrity),
                }
            }
            PublicationCompletionResult::Reload { snapshot } => {
                // A reload never touches DurableState: the admission-time definition must still
                // be the exact installed Arc (identity, not structural equality), and the
                // returned Snapshot must carry the exact Session identity and the exact
                // installed Workspace revision before the actor atomically replaces the
                // WorkspaceSnapshot Arc while preserving the exact definition Arc.
                match expected {
                    ExpectedPublication::ReloadWorkspace { definition } => {
                        if !Arc::ptr_eq(self.current.definition(), definition)
                            || snapshot.session_id() != definition.session_id()
                            || snapshot.revision() != definition.workspace().revision()
                        {
                            CompletionHandling::Fatal(ActorFatality::Integrity)
                        } else {
                            CompletionHandling::Success(
                                SessionWorkspaceDefinitionOutcome::Reloaded {
                                    definition_revision: definition.revision(),
                                    workspace_revision: snapshot.revision(),
                                },
                                Some(snapshot),
                                None,
                            )
                        }
                    }
                    _ => CompletionHandling::Fatal(ActorFatality::Integrity),
                }
            }
        }
    }

    fn installation_fault_is_armed(&self) -> bool {
        #[cfg(test)]
        {
            self.hooks
                .fail_next_install_after_commit
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn finish_active_waiter(&self, waiter: &Arc<PublicationWaiterState>) {
        self.failure_state.clear(waiter);
        #[cfg(test)]
        self.hooks.settled.notify();
    }

    fn close_for_fatal(&mut self, _fatality: ActorFatality) {
        self.turn_admission_gate.close();
        self.closing.cancel();
        self.follow_up.clear();
        self.steer.clear();
        self.publish_queue_projection();
        self.pending_interactions.clear();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        // A security recovery worker is only running while the Session is Idle, so it cannot
        // be running when most other owners fatal; the one exception is a Turn completion task
        // failure that happens right after the terminal recovery start.  In that case the
        // state is kept so the drain loop reaps the worker completion (which settles the
        // waiters with Internal) and close never leaves the task behind.  A state without a
        // worker settles its waiters exactly once with Internal here.
        if let Some(state) = self.security_invalidation.take() {
            if state.worker_task.is_some() {
                self.security_invalidation = Some(state);
            } else {
                for waiter in state.waiters {
                    let _ = waiter.send(Err(
                        SessionSecurityInvalidationError::InternalDispatchUnavailable,
                    ));
                }
            }
        }
        if let Some(state) = self.prepare_unload.take() {
            for waiter in state.waiters {
                let _ = waiter.send(Err(SessionExecutorPrepareUnloadError::Internal));
            }
        }
        if let Some(active) = self.active_publication.take() {
            active.waiter.settle(Err(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            ));
            self.finish_active_waiter(&active.waiter);
        }
    }

    fn install_current_state(&mut self, execution_state: SessionExecutionState) {
        self.publish_execution_state(
            execution_state,
            self.current.current_turn(),
            self.current.last_terminal(),
        );
    }

    fn publish_execution_state(
        &mut self,
        execution_state: SessionExecutionState,
        current_turn: Option<TurnId>,
        last_terminal: Option<(TurnId, SessionTurnTerminal)>,
    ) {
        let (active_submit_command_id, follow_up_command_ids, steer_command_ids) =
            self.queue_projection(current_turn);
        let previous_execution_state = self.current.execution_state();
        let current = Arc::new(
            self.current
                .with_execution(execution_state, current_turn, last_terminal)
                .with_queue_projection(
                    active_submit_command_id,
                    follow_up_command_ids,
                    steer_command_ids,
                ),
        );
        self.publish_current(current);
        if execution_state == SessionExecutionState::Finishing
            && previous_execution_state != SessionExecutionState::Finishing
        {
            let _ = self
                .events
                .send(Arc::new(SessionExecutorEvent::ExecutionChanged {
                    timestamp: SystemClock.now(),
                    snapshot: Arc::clone(&self.current),
                }));
        }
    }

    fn queue_projection(
        &self,
        current_turn: Option<TurnId>,
    ) -> (Option<CommandId>, Arc<[CommandId]>, Arc<[CommandId]>) {
        let active_submit_command_id = if current_turn.is_none() {
            self.active_admission
                .as_ref()
                .map(|admission| admission.command_id)
        } else {
            None
        };
        // The public legal readiness matrix requires every queue to be empty while non-Ready.
        // A FollowUp retained across an Agent Disable stays in the actor queue (it is never
        // dropped) but is hidden from the projection until Ready, when the Enable handoff
        // starts it.
        let ready = matches!(self.current.readiness(), SessionReadinessView::Ready);
        let follow_up_command_ids = if ready {
            Arc::from(self.follow_up.command_ids())
        } else {
            Arc::from([])
        };
        let steer_command_ids = Arc::from(
            current_turn
                .map(|turn_id| self.steer.command_ids_for_turn(turn_id))
                .unwrap_or_default(),
        );
        (
            active_submit_command_id,
            follow_up_command_ids,
            steer_command_ids,
        )
    }

    /// Atomically replaces the loaded Session's current/published snapshot metadata and emits the
    /// Session-scope metadata event carrying that exact snapshot.  The actor is the sole writer of
    /// `self.current` and `published_snapshot`, so the swap and the event share one metadata value.
    /// The actor verifies the durable CAS result is exactly the checked successor of the installed
    /// metadata before installing it; otherwise the gate-coherent revision contract is broken and
    /// the actor must not overwrite its snapshot.
    fn publish_metadata(
        &mut self,
        request: &mut UpdateSessionMetadataRequest,
    ) -> Result<(), ActorFatality> {
        let current_revision = self.current.metadata().revision().get();
        let expected_revision = current_revision.checked_add(1);
        if expected_revision != Some(request.metadata.revision().get())
            || request.metadata.updated_at() != request.timestamp
        {
            return Err(ActorFatality::Integrity);
        }
        let current = Arc::new(self.current.with_metadata(Arc::clone(&request.metadata)));
        self.current = Arc::clone(&current);
        *lock(&self.published_snapshot) = Arc::clone(&current);
        let _ = self
            .events
            .send(Arc::new(SessionExecutorEvent::MetadataUpdated {
                timestamp: request.timestamp,
                command_id: request.command_id,
                snapshot: Arc::clone(&current),
            }));
        request.settle(Ok(current));
        Ok(())
    }

    fn publish_queue_projection(&mut self) {
        let (active_submit_command_id, follow_up_command_ids, steer_command_ids) =
            self.queue_projection(self.current.current_turn());
        let current = Arc::new(self.current.with_queue_projection(
            active_submit_command_id,
            follow_up_command_ids,
            steer_command_ids,
        ));
        self.publish_current(current);
    }

    fn publish_current(&mut self, current: Arc<SessionExecutorSnapshot>) {
        let current = if current.current_turn().is_some() {
            let (current_turn_view, active_items, pending_interactions) =
                self.capture_public_observation(current.current_turn());
            Arc::new(current.with_public_observation(
                current_turn_view,
                active_items,
                pending_interactions,
            ))
        } else {
            Arc::new(current.with_public_observation(None, Arc::from([]), Arc::from([])))
        };
        let (usage, recording, diagnostics) =
            capture_public_session_state(self.conversation.as_ref());
        let current = Arc::new(current.with_public_session_state(usage, recording, diagnostics));
        self.current = Arc::clone(&current);
        *lock(&self.published_snapshot) = current;
    }

    fn capture_public_observation(
        &self,
        turn_id: Option<TurnId>,
    ) -> (
        Option<CurrentTurnView>,
        Arc<[ItemView]>,
        Arc<[InteractionView]>,
    ) {
        let Some(turn_id) = turn_id else {
            return (None, Arc::from([]), Arc::from([]));
        };
        let Some(conversation) = self.conversation.as_ref() else {
            return (None, Arc::from([]), Arc::from([]));
        };
        let live_state = lock(&conversation.live_state);
        let entries = live_state
            .selected_entries()
            .iter()
            .filter(|entry| entry.turn_id() == turn_id)
            .cloned()
            .collect::<Vec<_>>();
        let completed_tools = entries
            .iter()
            .filter_map(|entry| match entry.body() {
                StoredEntryBody::ToolMessage(message) => Some(message.item_id()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut active_items = Vec::new();
        let mut has_started_tool = false;
        for entry in &entries {
            let created_at = entry.timestamp();
            match entry.body() {
                StoredEntryBody::UserMessage(message) => {
                    let body = message
                        .content()
                        .message()
                        .content()
                        .iter()
                        .map(|part| part.as_text())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if let Ok(content) =
                        ItemContentView::user_message(message.source(), body, Vec::new())
                    {
                        if let Ok(item) = ItemView::new(
                            message.item_id(),
                            turn_id,
                            ItemStatusView::Completed,
                            content,
                            created_at,
                            Some(created_at),
                        ) {
                            active_items.push(item);
                        }
                    }
                }
                StoredEntryBody::AssistantMessage(message) => {
                    for content in message.content() {
                        let (item_id, item_content) = match content {
                            StoredAssistantContent::Reasoning {
                                item_id,
                                content: _,
                            } => (*item_id, ItemContentView::reasoning("reasoning redacted")),
                            StoredAssistantContent::Text { item_id, text } => {
                                (*item_id, ItemContentView::agent_message(text.as_ref()))
                            }
                            StoredAssistantContent::ToolCall {
                                item_id,
                                tool_call_id,
                                name,
                                ..
                            } => {
                                has_started_tool |= !completed_tools.contains(item_id);
                                (
                                    *item_id,
                                    ItemContentView::tool_invocation(
                                        tool_call_id.as_str(),
                                        name.as_str(),
                                        "arguments redacted",
                                        Option::<&str>::None,
                                    ),
                                )
                            }
                        };
                        let Ok(item_content) = item_content else {
                            continue;
                        };
                        let completed = match content {
                            StoredAssistantContent::ToolCall { item_id, .. } => {
                                completed_tools.contains(item_id)
                            }
                            StoredAssistantContent::Reasoning { .. }
                            | StoredAssistantContent::Text { .. } => true,
                        };
                        let status = if completed {
                            ItemStatusView::Completed
                        } else {
                            ItemStatusView::Started
                        };
                        let completed_at = completed.then_some(created_at);
                        if let Ok(item) = ItemView::new(
                            item_id,
                            turn_id,
                            status,
                            item_content,
                            created_at,
                            completed_at,
                        ) {
                            active_items.push(item);
                        }
                    }
                }
                StoredEntryBody::ToolMessage(_)
                | StoredEntryBody::InteractionRequested(_)
                | StoredEntryBody::InteractionResolved(_)
                | StoredEntryBody::Compaction(_) => {}
            }
        }
        let pending_interactions = live_state
            .pending_interaction_facts()
            .iter()
            .map(|fact| {
                InteractionView::new(
                    *fact.request_id(),
                    *fact.turn_id(),
                    *fact.item_id(),
                    fact.request().clone(),
                )
            })
            .collect::<Vec<_>>();
        let phase = if pending_interactions.iter().any(|interaction| {
            matches!(
                interaction.request(),
                InteractionRequestView::ToolApproval(_)
            )
        }) {
            Some(TurnExecutionPhaseView::WaitingApproval)
        } else if !pending_interactions.is_empty() {
            Some(TurnExecutionPhaseView::WaitingForUserInput)
        } else if has_started_tool {
            Some(TurnExecutionPhaseView::ExecutingTools)
        } else {
            Some(
                self.active_turn
                    .as_ref()
                    .filter(|active_turn| active_turn.turn_id == turn_id)
                    .map(|active_turn| active_turn.phase)
                    .unwrap_or(TurnExecutionPhaseView::Sampling),
            )
        };
        let started_at = entries
            .first()
            .map(|entry| entry.timestamp())
            .unwrap_or_else(|| SystemClock.now());
        let current_turn = Some(CurrentTurnView::new(
            turn_id,
            TurnStatusView::Running,
            phase,
            started_at,
        ));
        (
            current_turn,
            active_items.into(),
            pending_interactions.into(),
        )
    }
}

#[derive(Default)]
struct OptionalUsageTotal {
    value: u64,
    seen: bool,
    overflowed: bool,
}

impl OptionalUsageTotal {
    fn add(&mut self, value: Option<u64>) {
        let Some(value) = value else {
            return;
        };
        self.seen = true;
        if self.overflowed {
            return;
        }
        match self.value.checked_add(value) {
            Some(total) => self.value = total,
            None => self.overflowed = true,
        }
    }

    const fn projected(&self) -> Option<u64> {
        if self.seen && !self.overflowed {
            Some(self.value)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
struct CostTotal {
    coefficient: u128,
    scale: u8,
    overflowed: bool,
}

impl CostTotal {
    const fn new(amount: MoneyAmount) -> Self {
        Self {
            coefficient: amount.coefficient(),
            scale: amount.scale(),
            overflowed: false,
        }
    }

    fn add(&mut self, amount: MoneyAmount) {
        if self.overflowed {
            return;
        }
        let scale = self.scale.max(amount.scale());
        let Some(left) = scale_amount(self.coefficient, self.scale, scale) else {
            self.overflowed = true;
            return;
        };
        let Some(right) = scale_amount(amount.coefficient(), amount.scale(), scale) else {
            self.overflowed = true;
            return;
        };
        let Some(coefficient) = left.checked_add(right) else {
            self.overflowed = true;
            return;
        };
        self.coefficient = coefficient;
        self.scale = scale;
    }

    fn amount(self) -> Option<MoneyAmount> {
        if self.overflowed {
            return None;
        }
        canonical_money_amount(self.coefficient, self.scale)
    }
}

#[derive(Default)]
struct UsageProjection {
    model_calls: u64,
    compaction_calls: u64,
    input_tokens: OptionalUsageTotal,
    output_tokens: OptionalUsageTotal,
    reasoning_tokens: OptionalUsageTotal,
    cache_read_tokens: OptionalUsageTotal,
    cache_write_tokens: OptionalUsageTotal,
    reported_costs: BTreeMap<CurrencyCode, CostTotal>,
}

impl UsageProjection {
    fn add_model_call(&mut self, usage: Option<&ModelUsage>) {
        self.model_calls = self
            .model_calls
            .checked_add(1)
            .expect("the conversation entry cap bounds model calls");
        self.add_usage(usage);
    }

    fn add_compaction_call(&mut self, usage: Option<&ModelUsage>) {
        self.compaction_calls = self
            .compaction_calls
            .checked_add(1)
            .expect("the conversation entry cap bounds compaction calls");
        self.add_usage(usage);
    }

    fn add_usage(&mut self, usage: Option<&ModelUsage>) {
        let Some(usage) = usage else {
            return;
        };
        self.input_tokens.add(usage.input_tokens());
        self.output_tokens.add(usage.output_tokens());
        self.reasoning_tokens.add(usage.reasoning_tokens());
        self.cache_read_tokens.add(usage.cache_read_tokens());
        self.cache_write_tokens.add(usage.cache_write_tokens());
        if let Some(cost) = usage.reported_cost().copied() {
            self.reported_costs
                .entry(cost.currency())
                .and_modify(|total| total.add(cost.amount()))
                .or_insert_with(|| CostTotal::new(cost.amount()));
        }
    }

    fn finish(self) -> (SessionUsageView, Vec<SessionDiagnosticView>) {
        let mut diagnostics = Vec::new();
        add_usage_overflow_diagnostic(
            &mut diagnostics,
            self.input_tokens.overflowed,
            "usage_input_tokens_overflow",
            "input token usage overflowed",
        );
        add_usage_overflow_diagnostic(
            &mut diagnostics,
            self.output_tokens.overflowed,
            "usage_output_tokens_overflow",
            "output token usage overflowed",
        );
        add_usage_overflow_diagnostic(
            &mut diagnostics,
            self.reasoning_tokens.overflowed,
            "usage_reasoning_tokens_overflow",
            "reasoning token usage overflowed",
        );
        add_usage_overflow_diagnostic(
            &mut diagnostics,
            self.cache_read_tokens.overflowed,
            "usage_cache_read_tokens_overflow",
            "cache read token usage overflowed",
        );
        add_usage_overflow_diagnostic(
            &mut diagnostics,
            self.cache_write_tokens.overflowed,
            "usage_cache_write_tokens_overflow",
            "cache write token usage overflowed",
        );

        let too_many_currencies = self.reported_costs.len() > 8;
        let mut reported_costs = Vec::new();
        for (currency, total) in self.reported_costs.into_iter().take(8) {
            match total.amount() {
                Some(amount) => reported_costs.push(Money::new(amount, currency)),
                None => diagnostics.push(session_diagnostic(
                    "usage_currency_overflow",
                    "reported cost overflowed for one currency",
                )),
            }
        }
        if too_many_currencies {
            diagnostics.push(session_diagnostic(
                "usage_currency_limit_exceeded",
                "reported costs exceeded the currency limit",
            ));
        }
        let usage = SessionUsageView::new(
            self.model_calls,
            self.compaction_calls,
            self.input_tokens.projected(),
            self.output_tokens.projected(),
            self.reasoning_tokens.projected(),
            self.cache_read_tokens.projected(),
            self.cache_write_tokens.projected(),
            reported_costs,
        )
        .expect("the bounded conversation projection satisfies public usage limits");
        (usage, diagnostics)
    }
}

fn capture_public_session_state(
    conversation: Option<&Arc<LoadedSessionConversation>>,
) -> (
    Option<SessionUsageView>,
    SessionRecordingState,
    Arc<[SessionDiagnosticView]>,
) {
    let Some(conversation) = conversation else {
        return (None, SessionRecordingState::Healthy, Arc::from([]));
    };
    let mut projection = UsageProjection::default();
    {
        let live_state = lock(&conversation.live_state);
        for entry in live_state.selected_entries() {
            match entry.body() {
                StoredEntryBody::AssistantMessage(message) => {
                    projection.add_model_call(message.usage());
                }
                StoredEntryBody::Compaction(compaction) => {
                    if let Some(call) = compaction.model_call() {
                        projection.add_compaction_call(call.usage());
                    }
                }
                StoredEntryBody::UserMessage(_)
                | StoredEntryBody::ToolMessage(_)
                | StoredEntryBody::InteractionRequested(_)
                | StoredEntryBody::InteractionResolved(_) => {}
            }
        }
    }
    let (usage, mut usage_diagnostics) = projection.finish();
    let recording_health = conversation.recorder.health();
    let recording = match &*recording_health {
        RecordingHealth::Healthy => SessionRecordingState::Healthy,
        RecordingHealth::Degraded { .. } => SessionRecordingState::Degraded,
    };
    let mut diagnostics = Vec::with_capacity(usage_diagnostics.len() + 1);
    if let RecordingHealth::Degraded {
        failed_entry_id,
        reason,
    } = &*recording_health
    {
        diagnostics.push(recording_diagnostic(*failed_entry_id, *reason));
    }
    diagnostics.append(&mut usage_diagnostics);
    (Some(usage), recording, diagnostics.into())
}

fn recording_diagnostic(
    failed_entry_id: Option<crate::wire::EntryId>,
    reason: SessionRecordingError,
) -> SessionDiagnosticView {
    let (code, message) = if failed_entry_id.is_none() {
        (
            "session_recording_initialization_failed",
            "session recording initialization failed",
        )
    } else {
        match reason {
            SessionRecordingError::Encode(_) | SessionRecordingError::EntryTooLarge => (
                "session_recording_encode_failed",
                "session recording encoding failed",
            ),
            SessionRecordingError::Runtime(_) => (
                "session_recording_outcome_unknown",
                "session recording outcome is unknown",
            ),
            SessionRecordingError::TargetInvariant
            | SessionRecordingError::MetadataUnavailable
            | SessionRecordingError::EntrySessionMismatch
            | SessionRecordingError::FileTooLarge
            | SessionRecordingError::WriteFailed => (
                "session_recording_append_failed",
                "session recording append failed",
            ),
        }
    };
    session_diagnostic(code, message)
}

fn scale_amount(coefficient: u128, from_scale: u8, to_scale: u8) -> Option<u128> {
    coefficient.checked_mul(10_u128.pow(u32::from(to_scale.checked_sub(from_scale)?)))
}

fn canonical_money_amount(mut coefficient: u128, mut scale: u8) -> Option<MoneyAmount> {
    while scale != 0 && coefficient % 10 == 0 {
        coefficient /= 10;
        scale -= 1;
    }
    let text = if scale == 0 {
        coefficient.to_string()
    } else {
        let scale = usize::from(scale);
        let mut digits = coefficient.to_string();
        if digits.len() <= scale {
            let mut padded = String::with_capacity(scale + 1);
            padded.extend(std::iter::repeat_n('0', scale + 1 - digits.len()));
            padded.push_str(&digits);
            digits = padded;
        }
        let split = digits.len() - scale;
        format!("{}.{}", &digits[..split], &digits[split..])
    };
    text.parse().ok()
}

fn add_usage_overflow_diagnostic(
    diagnostics: &mut Vec<SessionDiagnosticView>,
    overflowed: bool,
    code: &str,
    message: &str,
) {
    if overflowed {
        diagnostics.push(session_diagnostic(code, message));
    }
}

fn session_diagnostic(code: &str, message: &str) -> SessionDiagnosticView {
    SessionDiagnosticView::new_with_limits(code, message, ProtocolLimits::v1_0())
        .expect("owner-controlled diagnostics satisfy public limits")
}

fn valid_durable_definition_shape(
    head: &crate::durable_state::DurableSessionHead,
    returned: &SessionDefinition,
    expected: &SessionDefinition,
) -> bool {
    head.session_id() == expected.session_id()
        && returned.session_id() == expected.session_id()
        && head.current_definition_revision() == expected.revision()
        && returned.revision() == expected.revision()
        && returned.workspace().revision() == expected.workspace().revision()
}

/// Validates an Agent upgrade `Updated` durable outcome against the installed current definition,
/// the admitted expected revision and the request owner timestamp: the installed current
/// revision matching the admitted expected revision, exact checked successor revision, same
/// SessionId, same AgentId at a different Agent revision, exact Workspace (including
/// WorkspaceRevision), Model and Prompt selection unchanged, owner timestamp preserved, and the
/// head pointing at the returned revision.
fn valid_durable_agent_upgrade_shape(
    head: &crate::durable_state::DurableSessionHead,
    returned: &SessionDefinition,
    current: &SessionDefinition,
    expected_revision: SessionDefinitionRevision,
    owner_timestamp: Timestamp,
) -> bool {
    let Some(successor) = current.revision().get().checked_add(1) else {
        return false;
    };
    current.revision() == expected_revision
        && head.session_id() == current.session_id()
        && returned.session_id() == current.session_id()
        && head.current_definition_revision() == returned.revision()
        && returned.revision().get() == successor
        && returned.agent().agent_id() == current.agent().agent_id()
        && returned.agent().revision() != current.agent().revision()
        && returned.workspace() == current.workspace()
        && returned.model() == current.model()
        && returned.prompts() == current.prompts()
        && returned.created_at() == owner_timestamp
}

enum ExecutorCompletion {
    Publication(PublicationCompletion),
    Admission(AdmissionCompletion),
    InteractionRequested(InteractionRequestedCompletion),
    SteerSafePoint(SteerSafePointCompletion),
    TurnPhase(TurnPhaseCompletion),
    Turn(TurnCompletion),
    SecurityRecovery(SecurityRecoveryCompletion),
}

struct InteractionRequestedCompletion {
    turn_id: TurnId,
    timestamp: Timestamp,
    item_id: ItemId,
    tool_call_id: crate::tools::ToolCallId,
    request_id: RequestId,
    request: InteractionRequest,
    resolution_sender: oneshot::Sender<ResolvedInteraction>,
}

struct SteerSafePointCompletion {
    turn_id: TurnId,
    response: oneshot::Sender<Option<QueuedSteer>>,
    close_if_empty: bool,
    refresh_public_snapshot: bool,
}

struct TurnPhaseCompletion {
    turn_id: TurnId,
    phase: TurnExecutionPhaseView,
    response: oneshot::Sender<()>,
}

struct PublicationCompletion {
    permit: SessionDefinitionPublicationPermit,
    result: PublicationCompletionResult,
}

struct AdmissionCompletion {
    turn_id: TurnId,
    result: Result<Arc<TurnExecutionContext>, SessionSubmitError>,
}

struct TurnCompletion {
    turn_id: TurnId,
    terminal: SessionTurnTerminal,
}

/// One owner-tracked security Workspace recovery worker completion.  The worker re-resolves the
/// exact definition it was spawned with; the actor verifies that exact Arc identity (and the
/// returned snapshot SessionId/revision) before installing any final state.
struct SecurityRecoveryCompletion {
    timestamp: Timestamp,
    definition: Arc<SessionDefinition>,
    result: SecurityRecoveryResult,
}

/// The closed result of one security Workspace recovery worker.
#[allow(clippy::large_enum_variant)]
enum SecurityRecoveryResult {
    Snapshot(Arc<WorkspaceSnapshot>),
    Unavailable(SessionUnavailableView),
    Closing,
    Internal,
}

#[allow(clippy::large_enum_variant)]
enum PublicationCompletionResult {
    Durable {
        outcome: Result<DurableSessionDefinitionOutcome, DurableSessionDefinitionError>,
        snapshot: Option<Arc<WorkspaceSnapshot>>,
    },
    AgentUpgrade {
        outcome: Result<DurableSessionAgentUpgradeOutcome, DurableSessionAgentUpgradeError>,
    },
    Reload {
        snapshot: Arc<WorkspaceSnapshot>,
    },
    Error(SessionDefinitionPublicationError),
}

enum CompletionHandling {
    Success(
        SessionWorkspaceDefinitionOutcome,
        Option<Arc<WorkspaceSnapshot>>,
        Option<Arc<SessionDefinition>>,
    ),
    Ordinary(SessionWorkspaceDefinitionError),
    Fatal(ActorFatality),
}

struct AdmissionWork {
    closing: CancellationToken,
    cancellation: CancellationToken,
    durable_state: DurableState,
    definition: Arc<SessionDefinition>,
    workspace: Arc<WorkspaceSnapshot>,
    prompt_service: Arc<PromptService>,
    resources: TurnResources,
    conversation: Arc<LoadedSessionConversation>,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    turn_id: TurnId,
    intent: PromptIntent,
    security_revocation: CancellationToken,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

async fn run_admission(
    work: AdmissionWork,
) -> Result<Arc<TurnExecutionContext>, SessionSubmitError> {
    let AdmissionWork {
        closing,
        cancellation,
        durable_state,
        definition,
        workspace,
        prompt_service,
        resources,
        conversation,
        turn_admission_gate,
        turn_id,
        intent,
        security_revocation,
        #[cfg(test)]
        hooks,
    } = work;
    if closing.is_cancelled() {
        return Err(SessionSubmitError::Closing);
    }
    if security_revocation.is_cancelled() {
        return Err(SessionSubmitError::SecurityRevoked);
    }
    if cancellation.is_cancelled() {
        return Err(SessionSubmitError::Cancelled);
    }
    let agent_read = durable_state.read_agent_definition(definition.agent());
    tokio::pin!(agent_read);
    let agent = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = security_revocation.cancelled() => return Err(SessionSubmitError::SecurityRevoked),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = &mut agent_read => result,
    }
    .map_err(map_agent_definition_read_error)?;
    let context = TurnExecutionContext::capture(TurnContextCapture {
        turn_id,
        session: definition,
        agent,
        workspace,
        prompt_service,
        prompt_resources: resources.prompt_resources,
        model_gateway: resources.model_gateway,
        model_catalog: resources.model_catalog,
        tool_set: resources.tool_set,
        compaction: resources.compaction,
    })
    .map_err(map_turn_context_capture_error)?;
    let message = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = security_revocation.cancelled() => return Err(SessionSubmitError::SecurityRevoked),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = context.resolve_user_message(intent) => result.map_err(map_submit_prompt_error)?,
    };
    if lock(&conversation.live_state).session_id() != context.session_id() {
        return Err(SessionSubmitError::InternalDispatchUnavailable);
    }
    if closing.is_cancelled() {
        return Err(SessionSubmitError::Closing);
    }
    if security_revocation.is_cancelled() {
        return Err(SessionSubmitError::SecurityRevoked);
    }
    if cancellation.is_cancelled() {
        return Err(SessionSubmitError::Cancelled);
    }
    let item_id = ItemId::generate().map_err(map_id_generation_error)?;
    let admission = tokio::select! {
        biased;
        _ = closing.cancelled() => return Err(SessionSubmitError::Closing),
        _ = security_revocation.cancelled() => return Err(SessionSubmitError::SecurityRevoked),
        _ = cancellation.cancelled() => return Err(SessionSubmitError::Cancelled),
        result = durable_state.acquire_agent_admission(context.agent()) => {
            result.map_err(map_agent_admission_error)?
        }
    };
    #[cfg(test)]
    hooks.after_agent_admission_before_input().await;
    let fact = {
        let _admission = admission;
        let _turn_admission = turn_admission_gate
            .try_enter()
            .ok_or(SessionSubmitError::Closing)?;
        if closing.is_cancelled() {
            return Err(SessionSubmitError::Closing);
        }
        if security_revocation.is_cancelled() {
            return Err(SessionSubmitError::SecurityRevoked);
        }
        if cancellation.is_cancelled() {
            return Err(SessionSubmitError::Cancelled);
        }
        let mut live_state = lock(&conversation.live_state);
        live_state
            .apply_user_message(
                StoredUserMessage::reconstruct(item_id, UserMessageSource::Input, message),
                turn_id,
                SystemClock.now(),
            )
            .map_err(|_| SessionSubmitError::InternalDispatchUnavailable)?
    };
    let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
    #[cfg(test)]
    hooks.after_input_before_completion().await;
    Ok(context)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one ActiveTurn binds its immutable context, model, channels, and cancellation basis"
)]
async fn run_active_turn(
    context: Arc<TurnExecutionContext>,
    model_gateway: Arc<ModelGateway>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    emergency_control: EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
    interaction_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    steer_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> SessionTurnTerminal {
    if context.turn_id() != turn_id {
        return SessionTurnTerminal::Failed(SessionTurnFailure::Internal);
    }
    let result = run_active_turn_inner(
        Arc::clone(&context),
        model_gateway,
        Arc::clone(&conversation),
        turn_id,
        control_generation,
        emergency_control.clone(),
        emergency_observation,
        cancellation.clone(),
        executor_closing,
        closing,
        interaction_completion_sender,
        steer_completion_sender,
        #[cfg(test)]
        hooks,
    )
    .await;
    match result {
        Ok(entry) => {
            let _ = conversation.recorder.record(entry).await;
            SessionTurnTerminal::Completed
        }
        Err(SessionTurnFailure::EmergencyControl(signal)) => {
            let settled = {
                let mut live_state = lock(&conversation.live_state);
                if live_state.current_turn() == Some(turn_id) {
                    live_state.fail_current_turn(turn_id)
                } else {
                    Ok(())
                }
            };
            if settled.is_err() {
                SessionTurnTerminal::Failed(SessionTurnFailure::Internal)
            } else {
                SessionTurnTerminal::Interrupted(session_turn_interruption(signal))
            }
        }
        Err(failure) => {
            let settled = {
                let mut live_state = lock(&conversation.live_state);
                if live_state.current_turn() == Some(turn_id) {
                    live_state.fail_current_turn(turn_id)
                } else {
                    Ok(())
                }
            };
            if settled.is_err() {
                SessionTurnTerminal::Failed(SessionTurnFailure::Internal)
            } else if cancellation.is_cancelled() {
                let Some(signal) = emergency_control
                    .observe(emergency_observation.target())
                    .and_then(|observation| observation.signal())
                else {
                    return SessionTurnTerminal::Failed(failure);
                };
                SessionTurnTerminal::Interrupted(session_turn_interruption(signal))
            } else {
                SessionTurnTerminal::Failed(failure)
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one ActiveTurn binds its immutable context, model, channels, and cancellation basis"
)]
async fn run_active_turn_inner(
    context: Arc<TurnExecutionContext>,
    model_gateway: Arc<ModelGateway>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    emergency_control: EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
    interaction_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    steer_completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> Result<Arc<crate::conversation_storage::StoredSessionEntry>, SessionTurnFailure> {
    let mut compactions_started = 0_u8;
    let compaction = ActiveTurnCompaction {
        context: Arc::clone(&context),
        model_gateway: Arc::clone(&model_gateway),
        conversation: Arc::clone(&conversation),
        turn_id,
        control_generation: Arc::clone(&control_generation),
        emergency_control: emergency_control.clone(),
        emergency_observation,
        cancellation: cancellation.clone(),
        executor_closing: executor_closing.clone(),
        closing: closing.clone(),
        completion_sender: steer_completion_sender.clone(),
        #[cfg(test)]
        hooks: Arc::clone(&hooks),
    };
    loop {
        if cancellation.is_cancelled()
            || !emergency_control_is_unsignaled_current(&emergency_control, emergency_observation)
        {
            return Err(SessionTurnFailure::Model);
        }
        let captured = lock(&conversation.live_state)
            .capture_conversation_views()
            .map_err(|_| SessionTurnFailure::Internal)?;
        if matches!(
            context.compaction_pressure(
                captured.compaction_source(),
                CompactionTrigger::ProactivePressure,
                compactions_started,
            ),
            CompactionPressure::Recommended
        ) && compaction
            .execute(
                &mut compactions_started,
                Arc::clone(captured.compaction_source()),
                CompactionTrigger::ProactivePressure,
            )
            .await?
        {
            continue;
        }
        let source_revision = captured.conversation().revision();
        let assembled = match context.assemble_agent_run(captured.conversation()) {
            Ok(assembled) => assembled,
            Err(error) if error.kind() == PromptErrorKind::ContextLimitExceeded => {
                compaction
                    .execute(
                        &mut compactions_started,
                        Arc::clone(captured.compaction_source()),
                        CompactionTrigger::PromptContextOverflow,
                    )
                    .await?;
                continue;
            }
            Err(error) => return Err(map_turn_prompt_error(error)),
        };
        let request = ModelCallRequest::new(
            Arc::clone(context.model()),
            ModelCallPurpose::AgentRun,
            assembled,
            source_revision,
            None,
        )
        .map(Arc::new)
        .map_err(map_model_request_error)?;
        #[cfg(test)]
        hooks.before_agent_run_attempt().await;
        let call_result = call_agent_run_with_logical_retry(
            &model_gateway,
            Arc::clone(&request),
            &conversation,
            turn_id,
            Arc::clone(&control_generation),
            emergency_control.clone(),
            emergency_observation,
            cancellation.clone(),
            executor_closing.clone(),
            closing.clone(),
        )
        .await;
        let (result, logical_retry_count) = match call_result {
            Ok(success) => success,
            Err(error) if error.reason() == ModelCallErrorReason::ContextOverflow => {
                if !cancellation.is_cancelled()
                    && let Some(signal) = emergency_control
                        .observe(emergency_observation.target())
                        .and_then(|observation| observation.signal())
                {
                    return Err(SessionTurnFailure::EmergencyControl(signal));
                }
                compaction
                    .execute(
                        &mut compactions_started,
                        Arc::clone(captured.compaction_source()),
                        CompactionTrigger::ProviderContextOverflow,
                    )
                    .await?;
                continue;
            }
            Err(error) => {
                if !cancellation.is_cancelled()
                    && let Some(signal) = emergency_control
                        .observe(emergency_observation.target())
                        .and_then(|observation| observation.signal())
                {
                    return Err(SessionTurnFailure::EmergencyControl(signal));
                }
                return Err(map_model_call_failure(error));
            }
        };
        if cancellation.is_cancelled() {
            return Err(SessionTurnFailure::Model);
        }
        let response = result.response();
        let mut content = Vec::with_capacity(response.content().len());
        let mut calls = Vec::new();
        for block in response.content() {
            let item_id = ItemId::generate().map_err(|_| SessionTurnFailure::Internal)?;
            match block {
                FinalizedAssistantContent::Reasoning(reasoning) => {
                    content.push(StoredAssistantContent::Reasoning {
                        item_id,
                        content: reasoning.clone(),
                    });
                }
                FinalizedAssistantContent::Text { text } => {
                    content.push(StoredAssistantContent::Text {
                        item_id,
                        text: Arc::clone(text),
                    });
                }
                FinalizedAssistantContent::ToolCall {
                    tool_call_id,
                    name,
                    arguments,
                } => {
                    let call_index =
                        u32::try_from(calls.len()).map_err(|_| SessionTurnFailure::Internal)?;
                    content.push(StoredAssistantContent::ToolCall {
                        item_id,
                        tool_call_id: tool_call_id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                    calls.push((
                        item_id,
                        ToolCall::new(
                            tool_call_id.clone(),
                            name.clone(),
                            arguments.clone(),
                            call_index,
                        ),
                    ));
                }
            }
        }
        let is_tool_round = !calls.is_empty();
        let disposition = if is_tool_round {
            AssistantDisposition::Intermediate
        } else {
            AssistantDisposition::Final
        };
        let body = StoredAssistantMessage::reconstruct(
            disposition,
            content,
            response.model().clone(),
            response.response_id().cloned(),
            response.finish_reason(),
            response.effective_max_output_tokens(),
            response.usage().cloned(),
            logical_retry_count,
            response.metadata().clone(),
        )
        .map_err(|_| SessionTurnFailure::Internal)?;
        let steer = if !is_tool_round {
            arbitrate_one_steer(
                Arc::clone(&conversation),
                turn_id,
                &emergency_control,
                emergency_observation,
                cancellation.clone(),
                &steer_completion_sender,
                true,
                false,
                #[cfg(test)]
                Arc::clone(&hooks),
            )
            .await?
        } else {
            SteerArbitration::none()
        };
        if let Some(queued) = steer.queued {
            if !emergency_control_is_unsignaled_current(&emergency_control, emergency_observation) {
                return Err(SessionTurnFailure::Model);
            }
            let intermediate = StoredAssistantMessage::reconstruct(
                AssistantDisposition::Intermediate,
                body.content().to_vec(),
                body.model().clone(),
                body.response_id().cloned(),
                body.finish_reason(),
                body.effective_max_output_tokens(),
                body.usage().cloned(),
                body.logical_retry_count(),
                body.metadata().clone(),
            )
            .map_err(|_| SessionTurnFailure::Internal)?;
            let assistant_fact = lock(&conversation.live_state)
                .apply_assistant_message(intermediate, turn_id, SystemClock.now())
                .map_err(|_| SessionTurnFailure::Internal)?;
            let _ = conversation
                .recorder
                .record(Arc::clone(assistant_fact.entry()))
                .await;
            if let Some(steer) = resolve_one_steer(
                Arc::clone(&context),
                Arc::clone(&conversation),
                turn_id,
                queued,
                assistant_fact.revision(),
                &emergency_control,
                emergency_observation,
                cancellation.clone(),
                #[cfg(test)]
                Arc::clone(&hooks),
            )
            .await?
            {
                if !emergency_control_is_unsignaled_current(
                    &emergency_control,
                    emergency_observation,
                ) {
                    return Err(SessionTurnFailure::Model);
                }
                let steer_fact = lock(&conversation.live_state)
                    .apply_user_message(steer, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?;
                let _ = conversation
                    .recorder
                    .record(Arc::clone(steer_fact.entry()))
                    .await;
            }
            continue;
        }
        if !emergency_control_is_unsignaled_current(&emergency_control, emergency_observation) {
            return Err(SessionTurnFailure::Model);
        }
        let fact = {
            let mut live_state = lock(&conversation.live_state);
            if is_tool_round {
                live_state
                    .apply_assistant_message(body, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?
            } else {
                live_state
                    .complete_with_assistant_message(body, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?
            }
        };
        let entry = Arc::clone(fact.entry());
        if !is_tool_round {
            return Ok(entry);
        }
        let _ = conversation.recorder.record(entry).await;

        if cancellation.is_cancelled()
            || !emergency_control_is_unsignaled_current(&emergency_control, emergency_observation)
        {
            lock(&conversation.live_state)
                .abandon_current_tool_exchange(turn_id)
                .map_err(|_| SessionTurnFailure::Internal)?;
            return Err(SessionTurnFailure::Model);
        }

        let requests = calls
            .into_iter()
            .map(|(item_id, call)| ToolExecutionRequest::new(item_id, call))
            .collect::<Vec<_>>();
        let tool_results = context.tool_set().execute_round(requests).await;
        let mut abandoned = false;
        for outcome in tool_results {
            let outcome = match outcome {
                ToolExecutionOutcome::Interaction {
                    item_id,
                    tool_call_id,
                    request_id,
                    request,
                    resolution_sender,
                    resolution_receiver,
                    allowed,
                    denied,
                } => {
                    let completion = InteractionRequestedCompletion {
                        turn_id,
                        timestamp: SystemClock.now(),
                        item_id,
                        tool_call_id: tool_call_id.clone(),
                        request_id,
                        request,
                        resolution_sender,
                    };
                    if interaction_completion_sender
                        .send(ExecutorCompletion::InteractionRequested(completion))
                        .is_err()
                    {
                        ToolExecutionOutcome::Abandoned {
                            item_id,
                            tool_call_id,
                            reason: crate::tools::ToolAbandonReason::RuntimeFailure,
                        }
                    } else {
                        let resolution = resolution_receiver.await;
                        match resolution {
                            Ok(resolution) => ToolSet::settle_interaction(
                                item_id,
                                tool_call_id,
                                *allowed,
                                *denied,
                                resolution,
                            ),
                            Err(_) => ToolExecutionOutcome::Abandoned {
                                item_id,
                                tool_call_id,
                                reason: crate::tools::ToolAbandonReason::RuntimeFailure,
                            },
                        }
                    }
                }
                outcome => outcome,
            };
            let (item_id, tool_call_id, stored) = stored_tool_outcome(outcome)?;
            abandoned |= matches!(&stored, StoredToolOutcome::Abandoned { .. });
            let fact = lock(&conversation.live_state)
                .apply_tool_message(
                    StoredToolMessage::reconstruct(item_id, tool_call_id, stored),
                    turn_id,
                    SystemClock.now(),
                )
                .map_err(|_| SessionTurnFailure::Internal)?;
            let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
        }
        if abandoned {
            lock(&conversation.live_state)
                .abandon_current_tool_exchange(turn_id)
                .map_err(|_| SessionTurnFailure::Internal)?;
            return Err(SessionTurnFailure::Model);
        }
        if cancellation.is_cancelled() {
            return Err(SessionTurnFailure::Model);
        }
        let steer = arbitrate_one_steer(
            Arc::clone(&conversation),
            turn_id,
            &emergency_control,
            emergency_observation,
            cancellation.clone(),
            &steer_completion_sender,
            false,
            false,
            #[cfg(test)]
            Arc::clone(&hooks),
        )
        .await?;
        if let Some(queued) = steer.queued {
            if let Some(steer) = resolve_one_steer(
                Arc::clone(&context),
                Arc::clone(&conversation),
                turn_id,
                queued,
                steer.basis_revision,
                &emergency_control,
                emergency_observation,
                cancellation.clone(),
                #[cfg(test)]
                Arc::clone(&hooks),
            )
            .await?
            {
                if !emergency_control_is_unsignaled_current(
                    &emergency_control,
                    emergency_observation,
                ) {
                    return Err(SessionTurnFailure::Model);
                }
                let fact = lock(&conversation.live_state)
                    .apply_user_message(steer, turn_id, SystemClock.now())
                    .map_err(|_| SessionTurnFailure::Internal)?;
                let _ = conversation.recorder.record(Arc::clone(fact.entry())).await;
            }
        }
    }
}

struct ActiveTurnCompaction {
    context: Arc<TurnExecutionContext>,
    model_gateway: Arc<ModelGateway>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    emergency_control: EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    #[cfg(test)]
    hooks: Arc<SessionExecutorTestHooksInner>,
}

struct ActiveCompactionOperation {
    session_id: SessionId,
    plan: Arc<CompactionPlan>,
    request: Arc<ModelCallRequest>,
}

impl ActiveTurnCompaction {
    async fn execute(
        &self,
        compactions_started: &mut u8,
        source: Arc<crate::compaction::LiveCompactionSourceView>,
        trigger: CompactionTrigger,
    ) -> Result<bool, SessionTurnFailure> {
        let applied = self
            .execute_operation(compactions_started, source, trigger)
            .await?;
        if applied {
            self.consume_one_steer().await?;
            self.publish_phase(TurnExecutionPhaseView::Sampling).await?;
        }
        Ok(applied)
    }

    async fn execute_operation(
        &self,
        compactions_started: &mut u8,
        source: Arc<crate::compaction::LiveCompactionSourceView>,
        trigger: CompactionTrigger,
    ) -> Result<bool, SessionTurnFailure> {
        let plan = match self
            .context
            .plan_compaction(source, trigger, *compactions_started)
        {
            Ok(plan) => plan,
            Err(_) if trigger == CompactionTrigger::ProactivePressure => return Ok(false),
            Err(_) => return Err(SessionTurnFailure::ContextOverflow),
        };
        let assembled = self
            .context
            .assemble_compaction(&plan)
            .map_err(map_turn_prompt_error)?;
        let request = ModelCallRequest::new(
            Arc::clone(self.context.model()),
            ModelCallPurpose::CompactionSummary,
            assembled,
            *plan.source().revision(),
            Some(plan.budget().max_output_tokens()),
        )
        .map(Arc::new)
        .map_err(map_model_request_error)?;
        let operation = ActiveCompactionOperation {
            session_id: self.context.session_id(),
            plan,
            request,
        };
        *compactions_started = compactions_started
            .checked_add(1)
            .ok_or(SessionTurnFailure::Internal)?;

        if self.cancellation.is_cancelled()
            || self.closing.is_cancelled()
            || !self.operation_is_current(&operation)
        {
            return Err(SessionTurnFailure::Model);
        }
        self.publish_phase(TurnExecutionPhaseView::Compacting)
            .await?;
        let (result, logical_retry_count) = self
            .call_summary_with_logical_retry(&operation)
            .await
            .map_err(|error| {
                if !self.cancellation.is_cancelled()
                    && let Some(signal) = self
                        .emergency_control
                        .observe(self.emergency_observation.target())
                        .and_then(|observation| observation.signal())
                {
                    return SessionTurnFailure::EmergencyControl(signal);
                }
                map_model_call_failure(error)
            })?;
        #[cfg(test)]
        self.hooks.before_compaction_apply().await;
        if self.cancellation.is_cancelled()
            || self.closing.is_cancelled()
            || !self.operation_is_current(&operation)
        {
            return Err(SessionTurnFailure::Model);
        }
        let validated = Compaction
            .validate_summary(Arc::clone(&operation.plan), &result, logical_retry_count)
            .map_err(|_| SessionTurnFailure::Model)?;
        if !Arc::ptr_eq(validated.plan(), &operation.plan) {
            return Err(SessionTurnFailure::Internal);
        }
        let replacement = validated
            .into_replacement()
            .map_err(|_| SessionTurnFailure::Model)?;
        if self.cancellation.is_cancelled()
            || self.closing.is_cancelled()
            || !self.operation_is_current(&operation)
        {
            return Err(SessionTurnFailure::Model);
        }
        let fact = lock(&self.conversation.live_state)
            .apply_compaction(
                Arc::clone(operation.plan.source()),
                operation.plan.summarized_unit_count(),
                replacement,
                self.turn_id,
                SystemClock.now(),
            )
            .map_err(|_| SessionTurnFailure::Internal)?;
        let _ = self
            .conversation
            .recorder
            .record(Arc::clone(fact.entry()))
            .await;
        Ok(true)
    }

    async fn call_summary_with_logical_retry(
        &self,
        operation: &ActiveCompactionOperation,
    ) -> Result<(ModelCallResult, u8), ModelCallError> {
        let mut logical_retries = 0_u8;
        loop {
            if self.cancellation.is_cancelled()
                || self.closing.is_cancelled()
                || !self.operation_is_current(operation)
            {
                return Err(ModelCallError::cancelled());
            }
            let result = self
                .model_gateway
                .generate_model_turn(
                    Arc::clone(&operation.request),
                    ModelProgressPublisher::discard(),
                    self.cancellation.clone(),
                )
                .await;
            let error = match result {
                Ok(result) => {
                    if self.cancellation.is_cancelled()
                        || self.closing.is_cancelled()
                        || !self.operation_is_current(operation)
                    {
                        return Err(ModelCallError::cancelled());
                    }
                    return Ok((result, logical_retries));
                }
                Err(error) => error,
            };
            if logical_retries >= COMPACTION_SUMMARY_MAX_LOGICAL_RETRIES {
                return Err(error);
            }
            let Some(delay) = agent_run_retry_delay(&error, 0) else {
                return Err(error);
            };
            if !self.operation_is_current(operation) {
                return Err(error);
            }
            logical_retries += 1;
            tokio::select! {
                biased;
                _ = self.executor_closing.cancelled() => return Err(error),
                _ = self.closing.cancelled() => return Err(error),
                _ = self.cancellation.cancelled() => return Err(error),
                _ = self.emergency_control.cancelled(
                    self.emergency_observation.target(),
                    self.emergency_observation.epoch(),
                ) => return Err(error),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn operation_is_current(&self, operation: &ActiveCompactionOperation) -> bool {
        operation.plan.source().session_id() == &operation.session_id
            && operation.request.purpose() == ModelCallPurpose::CompactionSummary
            && operation.request.source_revision() == *operation.plan.source().revision()
            && retry_basis_is_current(
                &self.conversation,
                self.turn_id,
                &self.control_generation,
                operation.request.source_revision(),
                &self.emergency_control,
                self.emergency_observation,
            )
    }

    async fn consume_one_steer(&self) -> Result<(), SessionTurnFailure> {
        let steer = arbitrate_one_steer(
            Arc::clone(&self.conversation),
            self.turn_id,
            &self.emergency_control,
            self.emergency_observation,
            self.cancellation.clone(),
            &self.completion_sender,
            false,
            true,
            #[cfg(test)]
            Arc::clone(&self.hooks),
        )
        .await?;
        if let Some(queued) = steer.queued
            && let Some(steer) = resolve_one_steer(
                Arc::clone(&self.context),
                Arc::clone(&self.conversation),
                self.turn_id,
                queued,
                steer.basis_revision,
                &self.emergency_control,
                self.emergency_observation,
                self.cancellation.clone(),
                #[cfg(test)]
                Arc::clone(&self.hooks),
            )
            .await?
        {
            if !emergency_control_is_unsignaled_current(
                &self.emergency_control,
                self.emergency_observation,
            ) {
                return Err(SessionTurnFailure::Model);
            }
            let fact = lock(&self.conversation.live_state)
                .apply_user_message(steer, self.turn_id, SystemClock.now())
                .map_err(|_| SessionTurnFailure::Internal)?;
            let _ = self
                .conversation
                .recorder
                .record(Arc::clone(fact.entry()))
                .await;
        }
        Ok(())
    }

    async fn publish_phase(&self, phase: TurnExecutionPhaseView) -> Result<(), SessionTurnFailure> {
        let (response, waiter) = oneshot::channel();
        self.completion_sender
            .send(ExecutorCompletion::TurnPhase(TurnPhaseCompletion {
                turn_id: self.turn_id,
                phase,
                response,
            }))
            .map_err(|_| SessionTurnFailure::Internal)?;
        waiter.await.map_err(|_| SessionTurnFailure::Internal)
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "retry policy receives the immutable call plus its owner-local cancellation basis"
)]
async fn call_agent_run_with_logical_retry(
    model_gateway: &ModelGateway,
    request: Arc<ModelCallRequest>,
    conversation: &LoadedSessionConversation,
    turn_id: TurnId,
    control_generation: Arc<ControlGeneration>,
    emergency_control: EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    executor_closing: CancellationToken,
    closing: CancellationToken,
) -> Result<(ModelCallResult, u8), ModelCallError> {
    let mut logical_retries = 0_u8;
    loop {
        if cancellation.is_cancelled() || closing.is_cancelled() {
            return Err(ModelCallError::cancelled());
        }
        if !retry_basis_is_current(
            conversation,
            turn_id,
            &control_generation,
            request.source_revision(),
            &emergency_control,
            emergency_observation,
        ) {
            return Err(ModelCallError::cancelled());
        }
        let result = model_gateway
            .generate_model_turn(
                Arc::clone(&request),
                ModelProgressPublisher::discard(),
                cancellation.clone(),
            )
            .await;
        let error = match result {
            Ok(result) => {
                if cancellation.is_cancelled()
                    || closing.is_cancelled()
                    || !retry_basis_is_current(
                        conversation,
                        turn_id,
                        &control_generation,
                        request.source_revision(),
                        &emergency_control,
                        emergency_observation,
                    )
                {
                    return Err(ModelCallError::cancelled());
                }
                return Ok((result, logical_retries));
            }
            Err(error) => error,
        };
        let Some(delay) = agent_run_retry_delay(&error, usize::from(logical_retries)) else {
            return Err(error);
        };
        if !retry_basis_is_current(
            conversation,
            turn_id,
            &control_generation,
            request.source_revision(),
            &emergency_control,
            emergency_observation,
        ) {
            return Err(error);
        }
        logical_retries += 1;
        tokio::select! {
            biased;
            _ = executor_closing.cancelled() => {
                return Err(error);
            }
            _ = closing.cancelled() => {
                return Err(error);
            }
            _ = cancellation.cancelled() => {
                return Err(error);
            }
            _ = emergency_control.cancelled(
                emergency_observation.target(),
                emergency_observation.epoch(),
            ) => {
                return Err(error);
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn retry_basis_is_current(
    conversation: &LoadedSessionConversation,
    turn_id: TurnId,
    control_generation: &Arc<ControlGeneration>,
    source_revision: crate::live_conversation::ConversationRevision,
    emergency_control: &EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
) -> bool {
    if !emergency_control_is_unsignaled_current(emergency_control, emergency_observation) {
        return false;
    }
    if !conversation.has_control_generation(turn_id, control_generation) {
        return false;
    }
    let live_state = lock(&conversation.live_state);
    if live_state.current_turn() != Some(turn_id) {
        return false;
    }
    live_state
        .capture_conversation_views()
        .ok()
        .is_some_and(|views| views.conversation().revision() == source_revision)
}

fn emergency_control_is_unsignaled_current(
    emergency_control: &EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
) -> bool {
    emergency_control
        .observe(emergency_observation.target())
        .is_some_and(|current| {
            current.epoch() == emergency_observation.epoch() && current.signal().is_none()
        })
}

const fn session_turn_interruption(signal: EmergencyControlSignal) -> SessionTurnInterruption {
    match signal {
        EmergencyControlSignal::Cancel => SessionTurnInterruption::UserCancelled,
        EmergencyControlSignal::SecurityRevoked => SessionTurnInterruption::SecurityRevoked,
        EmergencyControlSignal::PrepareForUnload => SessionTurnInterruption::PrepareForUnload,
    }
}

fn agent_run_retry_delay(
    error: &ModelCallError,
    logical_retries: usize,
) -> Option<std::time::Duration> {
    let backoff = AGENT_RUN_RETRY_BACKOFFS.get(logical_retries).copied()?;
    if !matches!(
        error.delivery(),
        ProviderRequestDeliveryState::NotSent
            | ProviderRequestDeliveryState::RejectedBeforeExecution
    ) {
        return None;
    }
    match error.reason() {
        ModelCallErrorReason::Timeout
        | ModelCallErrorReason::TransportUnavailable
        | ModelCallErrorReason::ProviderUnavailable => Some(backoff),
        ModelCallErrorReason::RateLimited => error
            .retry_after()
            .filter(|hint| *hint <= std::time::Duration::from_secs(60))
            .map(|hint| backoff.max(hint)),
        _ => None,
    }
}

struct SteerArbitration {
    basis_revision: crate::live_conversation::ConversationRevision,
    queued: Option<QueuedSteer>,
}

impl SteerArbitration {
    fn none() -> Self {
        Self {
            basis_revision: crate::live_conversation::ConversationRevision::default(),
            queued: None,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one Steer safe point receives its actor-owned emergency and cancellation basis"
)]
async fn arbitrate_one_steer(
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    emergency_control: &EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    steer_completion_sender: &mpsc::UnboundedSender<ExecutorCompletion>,
    close_if_empty: bool,
    refresh_public_snapshot: bool,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> Result<SteerArbitration, SessionTurnFailure> {
    if !emergency_control_is_unsignaled_current(emergency_control, emergency_observation) {
        return Err(SessionTurnFailure::Model);
    }
    #[cfg(test)]
    hooks.before_steer_safe_point().await;
    let basis_revision = lock(&conversation.live_state)
        .capture_conversation_views()
        .map_err(|_| SessionTurnFailure::Internal)?
        .conversation()
        .revision();
    let (response, waiter) = oneshot::channel();
    steer_completion_sender
        .send(ExecutorCompletion::SteerSafePoint(
            SteerSafePointCompletion {
                turn_id,
                response,
                close_if_empty,
                refresh_public_snapshot,
            },
        ))
        .map_err(|_| SessionTurnFailure::Internal)?;
    let Some(queued) = (tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(SessionTurnFailure::Model),
        _ = emergency_control.cancelled(
            emergency_observation.target(),
            emergency_observation.epoch(),
        ) => return Err(SessionTurnFailure::Model),
        result = waiter => result.map_err(|_| SessionTurnFailure::Internal)?,
    }) else {
        return Ok(SteerArbitration {
            basis_revision,
            queued: None,
        });
    };
    if queued.turn_id() != turn_id {
        return Err(SessionTurnFailure::Internal);
    }
    if !emergency_control_is_unsignaled_current(emergency_control, emergency_observation) {
        return Err(SessionTurnFailure::Model);
    }
    Ok(SteerArbitration {
        basis_revision,
        queued: Some(queued),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "one Steer resolve receives its captured context and actor-owned control basis"
)]
async fn resolve_one_steer(
    context: Arc<TurnExecutionContext>,
    conversation: Arc<LoadedSessionConversation>,
    turn_id: TurnId,
    queued: QueuedSteer,
    basis_revision: crate::live_conversation::ConversationRevision,
    emergency_control: &EmergencyControlHandle,
    emergency_observation: EmergencyControlObservation,
    cancellation: CancellationToken,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> Result<Option<StoredUserMessage>, SessionTurnFailure> {
    let (_command_id, queued_turn_id, intent) = queued.into_parts();
    if queued_turn_id != turn_id {
        return Err(SessionTurnFailure::Internal);
    }
    if !emergency_control_is_unsignaled_current(emergency_control, emergency_observation) {
        return Err(SessionTurnFailure::Model);
    }
    let message = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(SessionTurnFailure::Model),
        _ = emergency_control.cancelled(
            emergency_observation.target(),
            emergency_observation.epoch(),
        ) => return Err(SessionTurnFailure::Model),
        result = context.resolve_user_message(intent) => result.map_err(map_turn_prompt_error)?,
    };
    #[cfg(test)]
    hooks.after_steer_resolution().await;
    if cancellation.is_cancelled()
        || !emergency_control_is_unsignaled_current(emergency_control, emergency_observation)
    {
        return Err(SessionTurnFailure::Model);
    }
    let current_revision = lock(&conversation.live_state)
        .capture_conversation_views()
        .map_err(|_| SessionTurnFailure::Internal)?
        .conversation()
        .revision();
    if current_revision != basis_revision {
        return Ok(None);
    }
    let item_id = ItemId::generate().map_err(|_| SessionTurnFailure::Internal)?;
    Ok(Some(StoredUserMessage::reconstruct(
        item_id,
        UserMessageSource::Steer,
        message,
    )))
}

fn map_model_call_failure(error: crate::model_gateway::ModelCallError) -> SessionTurnFailure {
    match error.reason() {
        ModelCallErrorReason::ContextOverflow => SessionTurnFailure::ContextOverflow,
        ModelCallErrorReason::Cancelled
        | ModelCallErrorReason::ModelUnavailable
        | ModelCallErrorReason::AuthMissing
        | ModelCallErrorReason::AuthRejected
        | ModelCallErrorReason::RateLimited
        | ModelCallErrorReason::QuotaExceeded
        | ModelCallErrorReason::UnsupportedCapability
        | ModelCallErrorReason::InvalidRequest
        | ModelCallErrorReason::SafetyBlocked
        | ModelCallErrorReason::Timeout
        | ModelCallErrorReason::TransportUnavailable
        | ModelCallErrorReason::ProviderUnavailable
        | ModelCallErrorReason::ProviderRejected
        | ModelCallErrorReason::RequestOutcomeUnknown
        | ModelCallErrorReason::StreamInterrupted
        | ModelCallErrorReason::UnexpectedToolCall
        | ModelCallErrorReason::InvalidStructuredOutput
        | ModelCallErrorReason::InvalidProviderResponse
        | ModelCallErrorReason::IncompleteResponse => SessionTurnFailure::Model,
    }
}

fn stored_tool_outcome(
    outcome: ToolExecutionOutcome,
) -> Result<(ItemId, crate::tools::ToolCallId, StoredToolOutcome), SessionTurnFailure> {
    match outcome {
        ToolExecutionOutcome::Completed {
            item_id,
            tool_call_id,
            source,
            disposition,
            content,
        } => StoredToolOutcome::completed(source, disposition, content)
            .map(|stored| (item_id, tool_call_id, stored))
            .map_err(|_| SessionTurnFailure::Internal),
        ToolExecutionOutcome::Abandoned {
            item_id,
            tool_call_id,
            reason,
        } => Ok((
            item_id,
            tool_call_id,
            StoredToolOutcome::Abandoned { reason },
        )),
        ToolExecutionOutcome::Interaction { .. } => Err(SessionTurnFailure::Internal),
    }
}

fn map_agent_definition_read_error(error: DurableAgentDefinitionReadError) -> SessionSubmitError {
    match error {
        DurableAgentDefinitionReadError::Closing => SessionSubmitError::Closing,
        DurableAgentDefinitionReadError::AgentNotFound
        | DurableAgentDefinitionReadError::RevisionUnavailable => {
            SessionSubmitError::AgentUnavailable
        }
        DurableAgentDefinitionReadError::StorageUnavailable => {
            SessionSubmitError::DependencyUnavailable
        }
        DurableAgentDefinitionReadError::InternalDispatchUnavailable => {
            SessionSubmitError::InternalDispatchUnavailable
        }
    }
}

fn map_turn_context_capture_error(error: TurnContextCaptureError) -> SessionSubmitError {
    match error {
        TurnContextCaptureError::InvalidBinding => SessionSubmitError::InternalDispatchUnavailable,
        TurnContextCaptureError::Model(_) => SessionSubmitError::DependencyUnavailable,
        TurnContextCaptureError::Prompt => SessionSubmitError::Prompt,
    }
}

fn map_submit_prompt_error(error: PromptError) -> SessionSubmitError {
    match error.kind() {
        PromptErrorKind::ContextLimitExceeded => SessionSubmitError::ContextOverflow,
        PromptErrorKind::SourceDiscovery
        | PromptErrorKind::ContentLoad
        | PromptErrorKind::DuplicateKey
        | PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing => SessionSubmitError::Prompt,
        PromptErrorKind::InvalidIntent | PromptErrorKind::InvalidContribution => {
            SessionSubmitError::InvalidArgument
        }
        PromptErrorKind::Internal => SessionSubmitError::InternalDispatchUnavailable,
    }
}

fn map_agent_admission_error(error: AgentAdmissionError) -> SessionSubmitError {
    match error {
        AgentAdmissionError::Closing => SessionSubmitError::Closing,
        AgentAdmissionError::AgentUnavailable => SessionSubmitError::AgentUnavailable,
    }
}

fn map_id_generation_error(_error: IdGenerationError) -> SessionSubmitError {
    SessionSubmitError::InternalDispatchUnavailable
}

fn map_turn_prompt_error(error: PromptError) -> SessionTurnFailure {
    match error.kind() {
        PromptErrorKind::ContextLimitExceeded => SessionTurnFailure::ContextOverflow,
        PromptErrorKind::SourceDiscovery
        | PromptErrorKind::ContentLoad
        | PromptErrorKind::DuplicateKey
        | PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution => SessionTurnFailure::Prompt,
        PromptErrorKind::Internal => SessionTurnFailure::Internal,
    }
}

fn map_model_request_error(
    error: crate::model_gateway::ModelRequestValidationError,
) -> SessionTurnFailure {
    match error.kind() {
        ModelRequestValidationErrorKind::AssemblyMismatch
        | ModelRequestValidationErrorKind::InvalidOutputLimit
        | ModelRequestValidationErrorKind::UnsupportedInput => SessionTurnFailure::Internal,
    }
}

async fn run_publication(
    context: WorkspacePublicationContext,
    session_id: SessionId,
    attempt: SealedSessionDefinitionAttempt,
    expected: ExpectedPublication,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> PublicationCompletionResult {
    if !expected.is_publish() {
        return PublicationCompletionResult::Durable {
            outcome: context
                .durable_state
                .update_session_definition(attempt)
                .await,
            snapshot: None,
        };
    }

    let snapshot = if expected.workspace_changed() {
        let candidate = match context
            .resolver
            .resolve(session_id, expected.definition().workspace())
            .await
        {
            Ok(candidate) => candidate,
            Err(error) => return PublicationCompletionResult::Error(map_workspace_error(error)),
        };
        if candidate.revision() != expected.definition().workspace().revision() {
            return PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            );
        }
        let skill_context = candidate.skill_capture_context();
        if !skill_context.roots().is_empty() {
            return PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            );
        }
        let prompt_context = candidate.prompt_capture_context();
        let requires_revalidation = !prompt_context.roots().is_empty();
        let capture = context
            .prompt_service
            .capture_workspace_sources(prompt_context);
        tokio::pin!(capture);
        let prompt_sources = match tokio::select! {
            biased;
            _ = context.cancelled() => return PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::Closing,
            ),
            result = &mut capture => result,
        } {
            Ok(sources) => sources,
            Err(error) => return PublicationCompletionResult::Error(map_prompt_error(error)),
        };
        if requires_revalidation {
            let revalidation_result = {
                let revalidation = context
                    .resolver
                    .revalidate_candidate(&candidate, expected.definition().workspace());
                tokio::pin!(revalidation);
                tokio::select! {
                    biased;
                    _ = context.cancelled() => return PublicationCompletionResult::Error(
                        SessionWorkspaceDefinitionError::Closing,
                    ),
                    result = &mut revalidation => result,
                }
            };
            match revalidation_result {
                Ok(true) => {}
                Ok(false) => {
                    return PublicationCompletionResult::Error(
                        SessionWorkspaceDefinitionError::WorkspaceUnavailable,
                    );
                }
                Err(error) => {
                    return PublicationCompletionResult::Error(map_workspace_error(error));
                }
            }
        }
        let skill_sources = Arc::from(Vec::new().into_boxed_slice());
        if context.is_cancelled() {
            return PublicationCompletionResult::Error(SessionWorkspaceDefinitionError::Closing);
        }
        let snapshot = match candidate.finish(prompt_sources, skill_sources) {
            Ok(snapshot) => snapshot,
            Err(WorkspaceSnapshotFinishError::AuthorizationMismatch) => {
                return PublicationCompletionResult::Error(
                    SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
                );
            }
        };

        #[cfg(test)]
        hooks.after_candidate_snapshot_finish_before_durable().await;
        Some(snapshot)
    } else {
        // Future-only Model/Prompt (or canonical-equivalent Workspace) replacement never invokes
        // the Workspace resolver and never builds a new snapshot.
        None
    };

    let outcome = context
        .durable_state
        .update_session_definition(attempt)
        .await;
    #[cfg(test)]
    if matches!(&outcome, Ok(DurableSessionDefinitionOutcome::Updated(..))) {
        hooks.after_commit_before_install().await;
    }
    PublicationCompletionResult::Durable { outcome, snapshot }
}

/// Publishes one explicit Session Agent revision upgrade.  It never invokes the Workspace
/// resolver and never captures Prompt or Skill sources: DurableState resolves target current and
/// validates retained membership and Agent status under its own Agent → Session publication
/// gates, and the exact candidate is validated by the actor only after the durable outcome
/// returns.
async fn run_agent_upgrade_publication(
    context: WorkspacePublicationContext,
    attempt: SealedSessionAgentUpgradeAttempt,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> PublicationCompletionResult {
    if context.is_cancelled() {
        return PublicationCompletionResult::Error(SessionDefinitionPublicationError::Closing);
    }
    let outcome = context.durable_state.upgrade_session_agent(attempt).await;
    #[cfg(test)]
    if matches!(&outcome, Ok(DurableSessionAgentUpgradeOutcome::Updated(..))) {
        hooks.after_commit_before_install().await;
    }
    PublicationCompletionResult::AgentUpgrade { outcome }
}

/// Reloads one loaded Session's installed Workspace.  It re-resolves the exact currently
/// installed definition Workspace, captures the Workspace Prompt sources, revalidates the
/// required authority, and finishes one exact WorkspaceSnapshot; it never calls DurableState and
/// never changes the durable definition, metadata, conversation, or Recorder.  The
/// resolve/capture/revalidate/finish body is the shared [`resolve_reload_workspace_snapshot`]
/// helper; this worker keeps the ordinary ReloadWorkspace error mapping (AuthorityDenied →
/// Unauthorized, shape failures → WorkspaceRejected, SourceDiscovery → WorkspaceUnavailable).
async fn run_workspace_reload(
    context: WorkspacePublicationContext,
    definition: Arc<SessionDefinition>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> PublicationCompletionResult {
    if context.is_cancelled() {
        return PublicationCompletionResult::Error(SessionDefinitionPublicationError::Closing);
    }
    let snapshot = {
        #[cfg(test)]
        let resolved =
            resolve_reload_workspace_snapshot(&context, definition.as_ref(), &hooks).await;
        #[cfg(not(test))]
        let resolved = resolve_reload_workspace_snapshot(&context, definition.as_ref()).await;
        match resolved {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return PublicationCompletionResult::Error(map_reload_recovery_error(error));
            }
        }
    };
    PublicationCompletionResult::Reload { snapshot }
}

/// The neutral classification of one shared Workspace resolve/capture/revalidate/finish attempt.
/// The ordinary ReloadWorkspace worker keeps its finer public mapping while the security
/// Workspace recovery worker collapses every resolver/validation failure into WorkspaceUnavailable
/// and every workspace Prompt capture failure into PromptUnavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceReloadErrorKind {
    Closing,
    WorkspaceUnavailable,
    AuthorityDenied,
    WorkspaceRejected,
    PromptUnavailable,
    PromptRejected,
    Internal,
}

/// The minimal shared resolve→capture→revalidate→finish body reused by the ordinary
/// ReloadWorkspace worker and the security Workspace recovery worker.  It never calls
/// DurableState and never changes the durable definition, metadata, conversation, or Recorder.
/// Every await races the executor closing token: resolve, capture, and revalidation each return
/// Closing on cancellation, so both workers are cancellable and `close()`/Unload can never wait
/// on a hung resolver or prompt service.
async fn resolve_reload_workspace_snapshot(
    context: &WorkspacePublicationContext,
    definition: &SessionDefinition,
    #[cfg(test)] hooks: &Arc<SessionExecutorTestHooksInner>,
) -> Result<Arc<WorkspaceSnapshot>, WorkspaceReloadErrorKind> {
    let resolve = context
        .resolver
        .resolve(definition.session_id(), definition.workspace());
    tokio::pin!(resolve);
    let candidate = match tokio::select! {
        biased;
        _ = context.cancelled() => return Err(WorkspaceReloadErrorKind::Closing),
        result = &mut resolve => result,
    } {
        Ok(candidate) => candidate,
        Err(error) => return Err(map_reload_resolve_error(error)),
    };
    if candidate.revision() != definition.workspace().revision() {
        return Err(WorkspaceReloadErrorKind::Internal);
    }
    let skill_context = candidate.skill_capture_context();
    if !skill_context.roots().is_empty() {
        return Err(WorkspaceReloadErrorKind::Internal);
    }
    let prompt_context = candidate.prompt_capture_context();
    let requires_revalidation = !prompt_context.roots().is_empty();
    let capture = context
        .prompt_service
        .capture_workspace_sources(prompt_context);
    tokio::pin!(capture);
    let prompt_sources = match tokio::select! {
        biased;
        _ = context.cancelled() => return Err(WorkspaceReloadErrorKind::Closing),
        result = &mut capture => result,
    } {
        Ok(sources) => sources,
        Err(error) => return Err(map_reload_prompt_error_kind(error)),
    };
    if requires_revalidation {
        let revalidation_result = {
            let revalidation = context
                .resolver
                .revalidate_candidate(&candidate, definition.workspace());
            tokio::pin!(revalidation);
            tokio::select! {
                biased;
                _ = context.cancelled() => return Err(WorkspaceReloadErrorKind::Closing),
                result = &mut revalidation => result,
            }
        };
        match revalidation_result {
            Ok(true) => {}
            Ok(false) => return Err(WorkspaceReloadErrorKind::WorkspaceUnavailable),
            Err(error) => return Err(map_reload_resolve_error(error)),
        }
    }
    let skill_sources = Arc::from(Vec::new().into_boxed_slice());
    if context.is_cancelled() {
        return Err(WorkspaceReloadErrorKind::Closing);
    }
    let snapshot = match candidate.finish(prompt_sources, skill_sources) {
        Ok(snapshot) => snapshot,
        Err(WorkspaceSnapshotFinishError::AuthorizationMismatch) => {
            return Err(WorkspaceReloadErrorKind::Internal);
        }
    };
    #[cfg(test)]
    hooks.after_candidate_snapshot_finish_before_durable().await;
    Ok(snapshot)
}

/// One security Workspace recovery worker.  It re-resolves the exact definition it was spawned
/// with through the shared helper and classifies per the security contract: every resolver
/// Root/authority/canonical/validation failure (including AuthorityDenied, which leaves the hard
/// restriction in the current policy) is WorkspaceUnavailable; every workspace Prompt
/// SourceDiscovery/ContentLoad/DuplicateKey failure is PromptUnavailable; Closing is typed
/// Closing; shape/channel/task mismatch (including non-empty Skill roots) is Internal.
async fn run_security_workspace_recovery(
    context: WorkspacePublicationContext,
    definition: Arc<SessionDefinition>,
    #[cfg(test)] hooks: Arc<SessionExecutorTestHooksInner>,
) -> SecurityRecoveryResult {
    if context.is_cancelled() {
        return SecurityRecoveryResult::Closing;
    }
    let resolved = {
        #[cfg(test)]
        let result =
            resolve_reload_workspace_snapshot(&context, definition.as_ref(), &hooks).await;
        #[cfg(not(test))]
        let result = resolve_reload_workspace_snapshot(&context, definition.as_ref()).await;
        result
    };
    match resolved {
        Ok(snapshot) => SecurityRecoveryResult::Snapshot(snapshot),
        Err(WorkspaceReloadErrorKind::Closing) => SecurityRecoveryResult::Closing,
        Err(
            WorkspaceReloadErrorKind::WorkspaceUnavailable
            | WorkspaceReloadErrorKind::AuthorityDenied
            | WorkspaceReloadErrorKind::WorkspaceRejected,
        ) => SecurityRecoveryResult::Unavailable(SessionUnavailableView::WorkspaceUnavailable),
        Err(
            WorkspaceReloadErrorKind::PromptUnavailable
            | WorkspaceReloadErrorKind::PromptRejected,
        ) => SecurityRecoveryResult::Unavailable(SessionUnavailableView::PromptUnavailable),
        Err(WorkspaceReloadErrorKind::Internal) => SecurityRecoveryResult::Internal,
    }
}

fn map_reload_resolve_error(error: WorkspaceResolveError) -> WorkspaceReloadErrorKind {
    match error {
        WorkspaceResolveError::Closing => WorkspaceReloadErrorKind::Closing,
        WorkspaceResolveError::RootUnavailable
        | WorkspaceResolveError::AuthorityUnavailable
        | WorkspaceResolveError::CanonicalizationFailed => {
            WorkspaceReloadErrorKind::WorkspaceUnavailable
        }
        WorkspaceResolveError::AuthorityDenied => WorkspaceReloadErrorKind::AuthorityDenied,
        WorkspaceResolveError::RootNotDirectory
        | WorkspaceResolveError::DuplicateRoot
        | WorkspaceResolveError::OverlappingRoots
        | WorkspaceResolveError::CwdOutsideRoots
        | WorkspaceResolveError::CwdRootMismatch => WorkspaceReloadErrorKind::WorkspaceRejected,
        WorkspaceResolveError::InternalDispatchUnavailable => WorkspaceReloadErrorKind::Internal,
    }
}

fn map_reload_prompt_error_kind(error: PromptError) -> WorkspaceReloadErrorKind {
    match error.kind() {
        PromptErrorKind::SourceDiscovery => WorkspaceReloadErrorKind::PromptUnavailable,
        PromptErrorKind::ContentLoad | PromptErrorKind::DuplicateKey => {
            WorkspaceReloadErrorKind::PromptRejected
        }
        PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution
        | PromptErrorKind::ContextLimitExceeded
        | PromptErrorKind::Internal => WorkspaceReloadErrorKind::Internal,
    }
}

/// Maps one shared recovery classification to the ordinary ReloadWorkspace public error
/// contract: AuthorityDenied → Unauthorized, shape/validation failures → WorkspaceRejected,
/// workspace Prompt SourceDiscovery → WorkspaceUnavailable, ContentLoad/DuplicateKey →
/// WorkspaceRejected, Closing/Internal unchanged.
fn map_reload_recovery_error(error: WorkspaceReloadErrorKind) -> SessionDefinitionPublicationError {
    match error {
        WorkspaceReloadErrorKind::Closing => SessionDefinitionPublicationError::Closing,
        WorkspaceReloadErrorKind::WorkspaceUnavailable => {
            SessionDefinitionPublicationError::WorkspaceUnavailable
        }
        WorkspaceReloadErrorKind::AuthorityDenied => SessionDefinitionPublicationError::Unauthorized,
        WorkspaceReloadErrorKind::WorkspaceRejected => {
            SessionDefinitionPublicationError::WorkspaceRejected
        }
        WorkspaceReloadErrorKind::PromptUnavailable => {
            SessionDefinitionPublicationError::WorkspaceUnavailable
        }
        WorkspaceReloadErrorKind::PromptRejected => {
            SessionDefinitionPublicationError::WorkspaceRejected
        }
        WorkspaceReloadErrorKind::Internal => {
            SessionDefinitionPublicationError::InternalDispatchUnavailable
        }
    }
}

fn map_prompt_error(error: PromptError) -> SessionWorkspaceDefinitionError {
    match error.kind() {
        PromptErrorKind::SourceDiscovery => SessionWorkspaceDefinitionError::WorkspaceUnavailable,
        PromptErrorKind::ContentLoad | PromptErrorKind::DuplicateKey => {
            SessionWorkspaceDefinitionError::WorkspaceRejected
        }
        PromptErrorKind::PromptUnavailable
        | PromptErrorKind::InvalidRole
        | PromptErrorKind::RequiredPromptMissing
        | PromptErrorKind::InvalidIntent
        | PromptErrorKind::InvalidContribution
        | PromptErrorKind::ContextLimitExceeded
        | PromptErrorKind::Internal => SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
    }
}

fn map_workspace_error(error: WorkspaceResolveError) -> SessionWorkspaceDefinitionError {
    match error {
        WorkspaceResolveError::Closing => SessionWorkspaceDefinitionError::Closing,
        WorkspaceResolveError::RootUnavailable | WorkspaceResolveError::AuthorityUnavailable => {
            SessionWorkspaceDefinitionError::WorkspaceUnavailable
        }
        WorkspaceResolveError::CanonicalizationFailed => {
            SessionWorkspaceDefinitionError::WorkspaceUnavailable
        }
        WorkspaceResolveError::InternalDispatchUnavailable => {
            SessionWorkspaceDefinitionError::InternalDispatchUnavailable
        }
        WorkspaceResolveError::RootNotDirectory
        | WorkspaceResolveError::DuplicateRoot
        | WorkspaceResolveError::OverlappingRoots
        | WorkspaceResolveError::CwdOutsideRoots
        | WorkspaceResolveError::CwdRootMismatch
        | WorkspaceResolveError::AuthorityDenied => {
            SessionWorkspaceDefinitionError::WorkspaceRejected
        }
    }
}

fn map_durable_definition_error(
    error: DurableSessionDefinitionError,
) -> SessionDefinitionPublicationError {
    match error {
        DurableSessionDefinitionError::Closing => SessionDefinitionPublicationError::Closing,
        DurableSessionDefinitionError::SessionNotFound => {
            SessionDefinitionPublicationError::SessionNotFound
        }
        DurableSessionDefinitionError::StaleRevision => {
            SessionDefinitionPublicationError::StaleRevision
        }
        DurableSessionDefinitionError::SessionArchived => {
            SessionDefinitionPublicationError::SessionArchived
        }
        DurableSessionDefinitionError::SessionDeleted => {
            SessionDefinitionPublicationError::SessionDeleted
        }
        DurableSessionDefinitionError::DurableStateTooLarge => {
            SessionDefinitionPublicationError::StateTooLarge
        }
        DurableSessionDefinitionError::StorageUnavailable => {
            SessionDefinitionPublicationError::StorageUnavailable
        }
        DurableSessionDefinitionError::InternalDispatchUnavailable => {
            SessionDefinitionPublicationError::InternalDispatchUnavailable
        }
    }
}

fn map_durable_agent_upgrade_error(
    error: DurableSessionAgentUpgradeError,
) -> SessionDefinitionPublicationError {
    match error {
        DurableSessionAgentUpgradeError::Closing => SessionDefinitionPublicationError::Closing,
        DurableSessionAgentUpgradeError::SessionNotFound => {
            SessionDefinitionPublicationError::SessionNotFound
        }
        DurableSessionAgentUpgradeError::StaleRevision => {
            SessionDefinitionPublicationError::StaleRevision
        }
        DurableSessionAgentUpgradeError::SessionArchived => {
            SessionDefinitionPublicationError::SessionArchived
        }
        DurableSessionAgentUpgradeError::SessionDeleted => {
            SessionDefinitionPublicationError::SessionDeleted
        }
        DurableSessionAgentUpgradeError::AgentMismatch => {
            SessionDefinitionPublicationError::AgentMismatch
        }
        DurableSessionAgentUpgradeError::AgentDisabled => {
            SessionDefinitionPublicationError::AgentDisabled
        }
        DurableSessionAgentUpgradeError::AgentDeleted => {
            SessionDefinitionPublicationError::AgentDeleted
        }
        DurableSessionAgentUpgradeError::RevisionUnavailable => {
            SessionDefinitionPublicationError::RevisionUnavailable
        }
        DurableSessionAgentUpgradeError::DurableStateTooLarge => {
            SessionDefinitionPublicationError::StateTooLarge
        }
        DurableSessionAgentUpgradeError::StorageUnavailable => {
            SessionDefinitionPublicationError::StorageUnavailable
        }
        DurableSessionAgentUpgradeError::InternalDispatchUnavailable => {
            SessionDefinitionPublicationError::InternalDispatchUnavailable
        }
    }
}

struct WorkspaceDefinitionRequest {
    expected_revision: SessionDefinitionRevision,
    workspace: Option<Workspace>,
    model: Option<SessionModelConfig>,
    prompts: Option<SessionPromptSelection>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
    candidate_cancellation: CancellationToken,
    response: Option<
        oneshot::Sender<Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>>,
    >,
}

impl WorkspaceDefinitionRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionWorkspaceDefinitionError::Closing));
    }
}

impl Drop for WorkspaceDefinitionRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct AgentUpgradeRequest {
    expected_revision: SessionDefinitionRevision,
    target: Option<AgentRevisionRef>,
    owner_timestamp: Timestamp,
    command_id: CommandId,
    candidate_cancellation: CancellationToken,
    response: Option<
        oneshot::Sender<
            Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError>,
        >,
    >,
}

impl AgentUpgradeRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionDefinitionPublicationError::Closing));
    }
}

impl Drop for AgentUpgradeRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct ReloadWorkspaceRequest {
    owner_timestamp: Timestamp,
    command_id: CommandId,
    candidate_cancellation: CancellationToken,
    response: Option<
        oneshot::Sender<
            Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError>,
        >,
    >,
}

impl ReloadWorkspaceRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionDefinitionPublicationOutcome, SessionDefinitionPublicationError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionDefinitionPublicationError::Closing));
    }
}

impl Drop for ReloadWorkspaceRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct UpdateSessionMetadataRequest {
    metadata: Arc<SessionMetadata>,
    timestamp: Timestamp,
    command_id: CommandId,
    response:
        Option<oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionMetadataPublishError>>>,
}

impl UpdateSessionMetadataRequest {
    fn settle(
        &mut self,
        outcome: Result<Arc<SessionExecutorSnapshot>, SessionMetadataPublishError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionMetadataPublishError::Closing));
    }
}

impl Drop for UpdateSessionMetadataRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct AgentAvailabilityRequest {
    agent_id: crate::wire::AgentId,
    available: bool,
    timestamp: Timestamp,
    command_id: CommandId,
    candidate_cancellation: CancellationToken,
    response: Option<oneshot::Sender<Result<(), SessionAgentAvailabilityError>>>,
}

impl AgentAvailabilityRequest {
    fn settle(&mut self, outcome: Result<(), SessionAgentAvailabilityError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionAgentAvailabilityError::Closing));
    }
}

impl Drop for AgentAvailabilityRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

/// One Runtime shared-resource installation request.  The exact definition the Runtime
/// precomputed availability against is carried as an Arc so the actor can validate by identity
/// that no definition publication replaced it; the precomputed model/selected-Prompt facts and
/// the new Prompt/Model roots arrive together.
struct SharedResourceUpdateRequest {
    expected_definition: Arc<SessionDefinition>,
    prompt_resources: Arc<PromptResourceView>,
    model_catalog: Arc<ModelCatalogView>,
    prompt_available: bool,
    model_available: bool,
    timestamp: Timestamp,
    command_id: CommandId,
    candidate_cancellation: CancellationToken,
    response: Option<oneshot::Sender<Result<(), SessionSharedResourceUpdateError>>>,
}

impl SharedResourceUpdateRequest {
    fn settle(&mut self, outcome: Result<(), SessionSharedResourceUpdateError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSharedResourceUpdateError::Closing));
    }
}

impl Drop for SharedResourceUpdateRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct SnapshotRequest {
    response:
        Option<oneshot::Sender<Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError>>>,
}

struct SubmitRequest {
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<TurnId, SessionSubmitError>>>,
}

struct FollowUpRequest {
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<(), SessionFollowUpError>>>,
}

struct SteerRequest {
    turn_id: TurnId,
    command_id: CommandId,
    intent: Option<PromptIntent>,
    response: Option<oneshot::Sender<Result<(), SessionSteerError>>>,
}

struct CancelQueuedMessageRequest {
    command_id: CommandId,
    response: Option<oneshot::Sender<Result<(), SessionQueuedMessageError>>>,
}

impl FollowUpRequest {
    fn settle(&mut self, outcome: Result<(), SessionFollowUpError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionFollowUpError::Closing));
    }
}

impl Drop for FollowUpRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SteerRequest {
    fn settle(&mut self, outcome: Result<(), SessionSteerError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSteerError::Closing));
    }
}

impl Drop for SteerRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl CancelQueuedMessageRequest {
    fn settle(&mut self, outcome: Result<(), SessionQueuedMessageError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionQueuedMessageError::Closing));
    }
}

impl Drop for CancelQueuedMessageRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct ResolveInteractionRequest {
    expected_turn_id: TurnId,
    item_id: ItemId,
    request_id: RequestId,
    resolution_key: InteractionResolutionKey,
    resolution: Option<InteractionResolutionInput>,
    timestamp: Timestamp,
    response: Option<oneshot::Sender<Result<(), SessionInteractionError>>>,
}

/// One graceful-Unload preparation.  The deadline is computed by the caller from the configured
/// grace; the actor joins it into the shared deadline state where the effective deadline only
/// shortens until it fires.  The waiter is retained by the actor until the executor is Idle (or
/// closes).
struct PrepareUnloadRequest {
    deadline: tokio::time::Instant,
    response: Option<oneshot::Sender<Result<(), SessionExecutorPrepareUnloadError>>>,
}

impl PrepareUnloadRequest {
    fn settle(&mut self, outcome: Result<(), SessionExecutorPrepareUnloadError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionExecutorPrepareUnloadError::Closing));
    }
}

impl Drop for PrepareUnloadRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct CancelRequest {
    target: SessionCancelTarget,
    timestamp: Timestamp,
    response: Option<oneshot::Sender<Result<SessionCancelAccepted, SessionCancelError>>>,
}

struct SecurityRevokedRequest {
    target: SessionCancelTarget,
    response: Option<oneshot::Sender<Result<(), SessionSecurityRevokedError>>>,
}

impl CancelRequest {
    fn settle(&mut self, outcome: Result<SessionCancelAccepted, SessionCancelError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionCancelError::Closing));
    }
}

impl Drop for CancelRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SecurityRevokedRequest {
    fn settle(&mut self, outcome: Result<(), SessionSecurityRevokedError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSecurityRevokedError::Closing));
    }
}

impl Drop for SecurityRevokedRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct SecurityInvalidationRequest {
    timestamp: Timestamp,
    response: Option<oneshot::Sender<Result<(), SessionSecurityInvalidationError>>>,
}

impl SecurityInvalidationRequest {
    fn settle(&mut self, outcome: Result<(), SessionSecurityInvalidationError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSecurityInvalidationError::Closing));
    }
}

impl Drop for SecurityInvalidationRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl ResolveInteractionRequest {
    fn settle(&mut self, outcome: Result<(), SessionInteractionError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionInteractionError::Closing));
    }
}

impl Drop for ResolveInteractionRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

struct SubscribeRequest {
    response:
        Option<oneshot::Sender<Result<SessionExecutorSubscription, SessionExecutorSnapshotError>>>,
}

impl SubscribeRequest {
    fn settle(
        &mut self,
        outcome: Result<SessionExecutorSubscription, SessionExecutorSnapshotError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionExecutorSnapshotError::Closing));
    }
}

impl Drop for SubscribeRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SubmitRequest {
    fn settle(&mut self, outcome: Result<TurnId, SessionSubmitError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionSubmitError::Closing));
    }
}

impl Drop for SubmitRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

impl SnapshotRequest {
    fn settle(
        &mut self,
        outcome: Result<Arc<SessionExecutorSnapshot>, SessionExecutorSnapshotError>,
    ) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionExecutorSnapshotError::Closing));
    }
}

impl Drop for SnapshotRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

#[cfg(test)]
struct StartingProbeRequest {
    response: Option<oneshot::Sender<Result<(), SessionWorkspaceDefinitionError>>>,
}

#[cfg(test)]
impl StartingProbeRequest {
    fn settle(&mut self, outcome: Result<(), SessionWorkspaceDefinitionError>) {
        if let Some(response) = self.response.take() {
            let _ = response.send(outcome);
        }
    }

    fn reject_closing(&mut self) {
        self.settle(Err(SessionWorkspaceDefinitionError::Closing));
    }
}

#[cfg(test)]
impl Drop for StartingProbeRequest {
    fn drop(&mut self) {
        self.reject_closing();
    }
}

enum SessionExecutorRequest {
    Update(WorkspaceDefinitionRequest),
    UpgradeAgent(AgentUpgradeRequest),
    ReloadWorkspace(ReloadWorkspaceRequest),
    PublishMetadata(UpdateSessionMetadataRequest),
    Snapshot(SnapshotRequest),
    Submit(SubmitRequest),
    FollowUp(FollowUpRequest),
    Steer(SteerRequest),
    CancelQueuedMessage(CancelQueuedMessageRequest),
    ResolveInteraction(ResolveInteractionRequest),
    Cancel(CancelRequest),
    SecurityRevoked(SecurityRevokedRequest),
    SecurityInvalidation(SecurityInvalidationRequest),
    Subscribe(SubscribeRequest),
    SetAgentAvailability(AgentAvailabilityRequest),
    UpdateSharedResources(SharedResourceUpdateRequest),
    PrepareUnload(PrepareUnloadRequest),
    #[cfg(test)]
    StartingProbe(StartingProbeRequest),
}

impl SessionExecutorRequest {
    fn reject_closing(&mut self) {
        match self {
            Self::Update(request) => request.reject_closing(),
            Self::UpgradeAgent(request) => request.reject_closing(),
            Self::ReloadWorkspace(request) => request.reject_closing(),
            Self::PublishMetadata(request) => request.reject_closing(),
            Self::Snapshot(request) => request.reject_closing(),
            Self::Submit(request) => request.reject_closing(),
            Self::FollowUp(request) => request.reject_closing(),
            Self::Steer(request) => request.reject_closing(),
            Self::CancelQueuedMessage(request) => request.reject_closing(),
            Self::ResolveInteraction(request) => request.reject_closing(),
            Self::Cancel(request) => request.reject_closing(),
            Self::SecurityRevoked(request) => request.reject_closing(),
            Self::SecurityInvalidation(request) => request.reject_closing(),
            Self::Subscribe(request) => request.reject_closing(),
            Self::SetAgentAvailability(request) => request.reject_closing(),
            Self::UpdateSharedResources(request) => request.reject_closing(),
            Self::PrepareUnload(request) => request.reject_closing(),
            #[cfg(test)]
            Self::StartingProbe(request) => request.reject_closing(),
        }
    }
}

#[derive(Default)]
struct ActorFailureState {
    active: Mutex<Option<Arc<PublicationWaiterState>>>,
    fatal: std::sync::atomic::AtomicBool,
}

impl ActorFailureState {
    fn mark_fatal(&self) {
        self.fatal.store(true, std::sync::atomic::Ordering::Release);
    }

    fn is_fatal(&self) -> bool {
        self.fatal.load(std::sync::atomic::Ordering::Acquire)
    }

    fn install(&self, waiter: Arc<PublicationWaiterState>) {
        let mut active = lock(&self.active);
        debug_assert!(
            active.is_none(),
            "one SessionExecutor has one active publication"
        );
        *active = Some(waiter);
    }

    fn clear(&self, waiter: &Arc<PublicationWaiterState>) {
        let mut active = lock(&self.active);
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, waiter))
        {
            *active = None;
        }
    }
}

struct PublicationWaiterState {
    sender: Mutex<
        Option<
            oneshot::Sender<
                Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
            >,
        >,
    >,
}

impl PublicationWaiterState {
    fn new(
        sender: oneshot::Sender<
            Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
        >,
    ) -> Self {
        Self {
            sender: Mutex::new(Some(sender)),
        }
    }

    fn settle(
        &self,
        outcome: Result<SessionWorkspaceDefinitionOutcome, SessionWorkspaceDefinitionError>,
    ) {
        let sender = lock(&self.sender).take();
        if let Some(sender) = sender {
            let _ = sender.send(outcome);
        }
    }
}

struct ActorExitGuard {
    closing: CancellationToken,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    failure_state: Arc<ActorFailureState>,
    turn_admission_gate: Arc<TurnAdmissionGate>,
    armed: bool,
}

impl ActorExitGuard {
    fn new(
        closing: CancellationToken,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
        failure_state: Arc<ActorFailureState>,
        turn_admission_gate: Arc<TurnAdmissionGate>,
    ) -> Self {
        Self {
            closing,
            task_context,
            durable_state,
            failure_state,
            turn_admission_gate,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActorExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.turn_admission_gate.close();
        self.closing.cancel();
        self.failure_state.mark_fatal();
        self.task_context.request_closing();
        self.durable_state.request_closing();
        let waiter = lock(&self.failure_state.active).take();
        if let Some(waiter) = waiter {
            waiter.settle(Err(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            ));
        }
    }
}

struct PublicationCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    permit: Option<SessionDefinitionPublicationPermit>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    settled: bool,
}

struct AdmissionCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    turn_id: TurnId,
    completed: bool,
}

impl AdmissionCompletionGuard {
    fn new(completion_sender: mpsc::UnboundedSender<ExecutorCompletion>, turn_id: TurnId) -> Self {
        Self {
            completion_sender,
            turn_id,
            completed: false,
        }
    }

    fn complete(&mut self, result: Result<Arc<TurnExecutionContext>, SessionSubmitError>) {
        self.completed = true;
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Admission(AdmissionCompletion {
                turn_id: self.turn_id,
                result,
            }));
    }
}

impl Drop for AdmissionCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Admission(AdmissionCompletion {
                turn_id: self.turn_id,
                result: Err(SessionSubmitError::InternalDispatchUnavailable),
            }));
    }
}

struct TurnCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    turn_id: TurnId,
    completed: bool,
}

impl TurnCompletionGuard {
    fn new(completion_sender: mpsc::UnboundedSender<ExecutorCompletion>, turn_id: TurnId) -> Self {
        Self {
            completion_sender,
            turn_id,
            completed: false,
        }
    }

    fn complete(&mut self, terminal: SessionTurnTerminal) {
        self.completed = true;
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Turn(TurnCompletion {
                turn_id: self.turn_id,
                terminal,
            }));
    }
}

impl Drop for TurnCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::Turn(TurnCompletion {
                turn_id: self.turn_id,
                terminal: SessionTurnTerminal::Failed(SessionTurnFailure::Internal),
            }));
    }
}

/// The RAII completion guard of one security Workspace recovery worker.  A normal completion
/// sends the exact result once; an unwinding or dropped worker sends one fallback completion on
/// drop — typed Closing when the shared task owner is already closing, otherwise Internal plus
/// closing both owners — so the actor always receives exactly one completion and the drain loop
/// can never wait forever.
struct SecurityRecoveryCompletionGuard {
    completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
    timestamp: Timestamp,
    definition: Option<Arc<SessionDefinition>>,
    task_context: RuntimeTaskContext,
    durable_state: DurableState,
    completed: bool,
}

impl SecurityRecoveryCompletionGuard {
    fn new(
        completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
        timestamp: Timestamp,
        definition: Arc<SessionDefinition>,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
    ) -> Self {
        Self {
            completion_sender,
            timestamp,
            definition: Some(definition),
            task_context,
            durable_state,
            completed: false,
        }
    }

    fn complete(&mut self, result: SecurityRecoveryResult) {
        if self.completed {
            return;
        }
        self.completed = true;
        let Some(definition) = self.definition.take() else {
            return;
        };
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::SecurityRecovery(SecurityRecoveryCompletion {
                timestamp: self.timestamp,
                definition,
                result,
            }));
    }
}

impl Drop for SecurityRecoveryCompletionGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        let Some(definition) = self.definition.take() else {
            return;
        };
        let result = if self.task_context.is_closing() {
            SecurityRecoveryResult::Closing
        } else {
            self.task_context.request_closing();
            self.durable_state.request_closing();
            SecurityRecoveryResult::Internal
        };
        let _ = self
            .completion_sender
            .send(ExecutorCompletion::SecurityRecovery(SecurityRecoveryCompletion {
                timestamp: self.timestamp,
                definition,
                result,
            }));
    }
}

impl PublicationCompletionGuard {
    fn new(
        completion_sender: mpsc::UnboundedSender<ExecutorCompletion>,
        permit: SessionDefinitionPublicationPermit,
        task_context: RuntimeTaskContext,
        durable_state: DurableState,
    ) -> Self {
        Self {
            completion_sender,
            permit: Some(permit),
            task_context,
            durable_state,
            settled: false,
        }
    }

    fn complete(&mut self, result: PublicationCompletionResult) {
        let permit = self
            .permit
            .take()
            .expect("a publication completion guard sends exactly once");
        let _ =
            self.completion_sender
                .send(ExecutorCompletion::Publication(PublicationCompletion {
                    permit,
                    result,
                }));
        self.settled = true;
    }
}

impl Drop for PublicationCompletionGuard {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let Some(permit) = self.permit.take() else {
            return;
        };
        let result = if self.task_context.is_closing() {
            PublicationCompletionResult::Error(SessionWorkspaceDefinitionError::Closing)
        } else {
            self.task_context.request_closing();
            self.durable_state.request_closing();
            PublicationCompletionResult::Error(
                SessionWorkspaceDefinitionError::InternalDispatchUnavailable,
            )
        };
        let _ =
            self.completion_sender
                .send(ExecutorCompletion::Publication(PublicationCompletion {
                    permit,
                    result,
                }));
        self.settled = true;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct SessionExecutorTestHooks {
    inner: Arc<SessionExecutorTestHooksInner>,
}

#[cfg(test)]
impl SessionExecutorTestHooks {
    pub(crate) fn arm_before_snapshot_response(&self) {
        self.inner.before_snapshot_response.arm();
    }

    pub(crate) async fn wait_before_snapshot_response(&self) {
        self.inner
            .before_snapshot_response
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_snapshot_response(&self) {
        self.inner.before_snapshot_response.release();
    }

    pub(crate) fn arm_after_agent_admission_before_input(&self) {
        self.inner.after_agent_admission_before_input.arm();
    }

    pub(crate) async fn wait_after_agent_admission_before_input(&self) {
        self.inner
            .after_agent_admission_before_input
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_agent_admission_before_input(&self) {
        self.inner.after_agent_admission_before_input.release();
    }

    pub(crate) fn arm_after_input_before_completion(&self) {
        self.inner.after_input_before_completion.arm();
    }

    pub(crate) async fn wait_after_input_before_completion(&self) {
        self.inner
            .after_input_before_completion
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_input_before_completion(&self) {
        self.inner.after_input_before_completion.release();
    }

    pub(crate) fn arm_before_agent_run_attempt(&self) {
        self.inner.before_agent_run_attempt.arm();
    }

    pub(crate) async fn wait_before_agent_run_attempt(&self) {
        self.inner
            .before_agent_run_attempt
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_agent_run_attempt(&self) {
        self.inner.before_agent_run_attempt.release();
    }

    pub(crate) fn arm_before_steer_safe_point(&self) {
        self.inner.before_steer_safe_point.arm();
    }

    pub(crate) fn arm_before_compaction_apply(&self) {
        self.inner.before_compaction_apply.arm();
    }

    pub(crate) async fn wait_before_compaction_apply(&self) {
        self.inner
            .before_compaction_apply
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_compaction_apply(&self) {
        self.inner.before_compaction_apply.release();
    }

    pub(crate) async fn wait_before_steer_safe_point(&self) {
        self.inner
            .before_steer_safe_point
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_before_steer_safe_point(&self) {
        self.inner.before_steer_safe_point.release();
    }

    pub(crate) fn arm_after_steer_resolution(&self) {
        self.inner.after_steer_resolution.arm();
    }

    pub(crate) async fn wait_after_steer_resolution(&self) {
        self.inner.after_steer_resolution.wait_until_entered().await;
    }

    pub(crate) fn release_after_steer_resolution(&self) {
        self.inner.after_steer_resolution.release();
    }

    pub(crate) fn arm_after_steer_arbitration(&self) {
        self.inner.after_steer_arbitration.arm();
    }

    pub(crate) async fn wait_after_steer_arbitration(&self) {
        self.inner
            .after_steer_arbitration
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_steer_arbitration(&self) {
        self.inner.after_steer_arbitration.release();
    }

    pub(crate) fn arm_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.arm();
    }

    pub(crate) async fn wait_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.wait_until_entered().await;
    }

    pub(crate) fn release_after_candidate_snapshot_finish_before_durable(&self) {
        self.inner.after_snapshot_finish.release();
    }

    pub(crate) fn arm_after_commit_before_install(&self) {
        self.inner.after_commit_before_install.arm();
    }

    pub(crate) async fn wait_after_commit_before_install(&self) {
        self.inner
            .after_commit_before_install
            .wait_until_entered()
            .await;
    }

    pub(crate) fn release_after_commit_before_install(&self) {
        self.inner.after_commit_before_install.release();
    }

    pub(crate) async fn wait_for_publication_settlement(&self) {
        self.inner.settled.wait().await;
    }

    pub(crate) fn fail_next_snapshot_install_after_commit(&self) {
        self.inner
            .fail_next_install_after_commit
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
struct SessionExecutorTestHooksInner {
    before_snapshot_response: Arc<NamedAsyncBarrier>,
    after_agent_admission_before_input: Arc<NamedAsyncBarrier>,
    after_input_before_completion: Arc<NamedAsyncBarrier>,
    before_agent_run_attempt: Arc<NamedAsyncBarrier>,
    before_compaction_apply: Arc<NamedAsyncBarrier>,
    before_steer_safe_point: Arc<NamedAsyncBarrier>,
    after_steer_resolution: Arc<NamedAsyncBarrier>,
    after_steer_arbitration: Arc<NamedAsyncBarrier>,
    after_snapshot_finish: Arc<NamedAsyncBarrier>,
    after_commit_before_install: Arc<NamedAsyncBarrier>,
    settled: Arc<SettlementNotification>,
    fail_next_install_after_commit: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl SessionExecutorTestHooksInner {
    fn new() -> Self {
        Self {
            before_snapshot_response: Arc::new(NamedAsyncBarrier::new()),
            after_agent_admission_before_input: Arc::new(NamedAsyncBarrier::new()),
            after_input_before_completion: Arc::new(NamedAsyncBarrier::new()),
            before_agent_run_attempt: Arc::new(NamedAsyncBarrier::new()),
            before_compaction_apply: Arc::new(NamedAsyncBarrier::new()),
            before_steer_safe_point: Arc::new(NamedAsyncBarrier::new()),
            after_steer_resolution: Arc::new(NamedAsyncBarrier::new()),
            after_steer_arbitration: Arc::new(NamedAsyncBarrier::new()),
            after_snapshot_finish: Arc::new(NamedAsyncBarrier::new()),
            after_commit_before_install: Arc::new(NamedAsyncBarrier::new()),
            settled: Arc::new(SettlementNotification::new()),
            fail_next_install_after_commit: std::sync::atomic::AtomicBool::new(false),
        }
    }

    async fn before_snapshot_response(&self) {
        self.before_snapshot_response.wait_if_armed().await;
    }

    async fn after_agent_admission_before_input(&self) {
        self.after_agent_admission_before_input
            .wait_if_armed()
            .await;
    }

    async fn after_input_before_completion(&self) {
        self.after_input_before_completion.wait_if_armed().await;
    }

    async fn before_agent_run_attempt(&self) {
        self.before_agent_run_attempt.wait_if_armed().await;
    }

    async fn before_compaction_apply(&self) {
        self.before_compaction_apply.wait_if_armed().await;
    }

    async fn before_steer_safe_point(&self) {
        self.before_steer_safe_point.wait_if_armed().await;
    }

    async fn after_steer_resolution(&self) {
        self.after_steer_resolution.wait_if_armed().await;
    }

    async fn after_steer_arbitration(&self) {
        self.after_steer_arbitration.wait_if_armed().await;
    }

    async fn after_candidate_snapshot_finish_before_durable(&self) {
        self.after_snapshot_finish.wait_if_armed().await;
    }

    async fn after_commit_before_install(&self) {
        self.after_commit_before_install.wait_if_armed().await;
    }
}

#[cfg(test)]
struct NamedAsyncBarrier {
    armed: std::sync::atomic::AtomicBool,
    entered: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl NamedAsyncBarrier {
    fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            entered: std::sync::atomic::AtomicBool::new(false),
            released: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn arm(&self) {
        self.entered
            .store(false, std::sync::atomic::Ordering::Release);
        self.released
            .store(false, std::sync::atomic::Ordering::Release);
        self.armed.store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait_if_armed(&self) {
        if self
            .armed
            .compare_exchange(
                true,
                false,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        self.entered
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    async fn wait_until_entered(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.entered.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }
}

#[cfg(test)]
struct SettlementNotification {
    count: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

#[cfg(test)]
impl SettlementNotification {
    fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn notify(&self) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let count = self.count.load(std::sync::atomic::Ordering::Acquire);
            if count != 0
                && self
                    .count
                    .compare_exchange(
                        count,
                        count - 1,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Acquire,
                    )
                    .is_ok()
            {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::future::{Future, poll_fn};
    use std::num::{NonZeroU8, NonZeroU32};
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::runtime::Handle;

    use crate::conversation_storage::{
        RecorderWriteBarrier, load_replayed_conversation_with_barrier_for_test,
    };
    use crate::durable_state::DurableState;
    use crate::model_gateway::{ModelSelection, ReasoningPreference, ScriptedModelFixture};
    use crate::prompt::{PromptBodyIntent, TextIntent};
    use crate::runtime_task::RuntimeTaskContext;
    use crate::wire::{CanonicalFileUri, FileUriFamily, RequestId, SessionId};
    use crate::workspace::{
        RequestedFilesystemAccess, WorkspaceCwdSpec, WorkspaceDefinitionInput, WorkspacePathTarget,
        WorkspaceRootInput, WorkspaceRootKey, WorkspaceSourcePolicy, lower_workspace,
    };

    const AGENT_ID: &str = "agt_11111111111111111111111111111111";
    const SESSION_ID: &str = "ses_22222222222222222222222222222222";
    const G1: &str = "00000000000000000001";
    const G2: &str = "00000000000000000002";

    static NEXT_TEST_ROOT: AtomicUsize = AtomicUsize::new(1);

    #[test]
    fn public_usage_projection_aggregates_tokens_costs_and_overflow_diagnostics() {
        let usd: CurrencyCode = "USD".parse().unwrap();
        let first = ModelUsage::reconstruct(
            Some(u64::MAX),
            Some(2),
            None,
            None,
            None,
            None,
            Some(Money::new("0.01".parse().unwrap(), usd)),
        );
        let second = ModelUsage::reconstruct(
            Some(1),
            Some(3),
            Some(4),
            None,
            None,
            None,
            Some(Money::new("0.09".parse().unwrap(), usd)),
        );
        let mut projection = UsageProjection::default();
        projection.add_model_call(Some(&first));
        projection.add_model_call(Some(&second));
        let (usage, diagnostics) = projection.finish();

        assert_eq!(usage.model_calls(), 2);
        assert_eq!(usage.input_tokens(), None);
        assert_eq!(usage.output_tokens(), Some(5));
        assert_eq!(usage.reasoning_tokens(), Some(4));
        assert_eq!(usage.reported_costs().len(), 1);
        assert_eq!(usage.reported_costs()[0].amount().to_string(), "0.1");
        assert_eq!(
            diagnostics
                .iter()
                .map(SessionDiagnosticView::code)
                .collect::<Vec<_>>(),
            ["usage_input_tokens_overflow"]
        );
    }

    #[test]
    fn public_usage_projection_bounds_currencies_and_diagnoses_decimal_overflow() {
        let mut projection = UsageProjection::default();
        for code in [
            "AAA", "AAB", "AAC", "AAD", "AAE", "AAF", "AAG", "AAH", "AAI",
        ] {
            let usage = ModelUsage::reconstruct(
                None,
                None,
                None,
                None,
                None,
                None,
                Some(Money::new("1".parse().unwrap(), code.parse().unwrap())),
            );
            projection.add_model_call(Some(&usage));
        }
        let (usage, diagnostics) = projection.finish();
        assert_eq!(usage.reported_costs().len(), 8);
        assert_eq!(
            usage
                .reported_costs()
                .iter()
                .map(|cost| cost.currency().to_string())
                .collect::<Vec<_>>(),
            vec!["AAA", "AAB", "AAC", "AAD", "AAE", "AAF", "AAG", "AAH"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == "usage_currency_limit_exceeded")
        );

        let maximum = "999999999999999999.999999999".parse().unwrap();
        let usd = "USD".parse().unwrap();
        let overflowing = ModelUsage::reconstruct(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Money::new(maximum, usd)),
        );
        let mut overflow_projection = UsageProjection::default();
        overflow_projection.add_model_call(Some(&overflowing));
        overflow_projection.add_model_call(Some(&overflowing));
        let (overflow_usage, overflow_diagnostics) = overflow_projection.finish();
        assert!(overflow_usage.reported_costs().is_empty());
        assert!(
            overflow_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == "usage_currency_overflow")
        );
    }

    async fn poll_once_pending<F>(mut future: Pin<&mut F>) -> bool
    where
        F: Future,
    {
        poll_fn(|context| {
            std::task::Poll::Ready(matches!(
                future.as_mut().poll(context),
                std::task::Poll::Pending
            ))
        })
        .await
    }

    struct TempStore {
        root: PathBuf,
        old_workspace: PathBuf,
        new_workspace: PathBuf,
    }

    impl TempStore {
        fn new() -> Self {
            loop {
                let number = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
                let root = std::env::temp_dir().join(format!(
                    "minicore-session-executor-store-{}-{number}",
                    std::process::id()
                ));
                if root.exists() {
                    continue;
                }
                fs::create_dir(&root).expect("the temporary Store root is created");
                set_private_directory_mode(&root);
                let old_workspace = root.with_file_name(format!(
                    "minicore-session-executor-workspace-old-{}-{number}",
                    std::process::id()
                ));
                let new_workspace = root.with_file_name(format!(
                    "minicore-session-executor-workspace-new-{}-{number}",
                    std::process::id()
                ));
                if old_workspace.exists() || new_workspace.exists() {
                    let _ = fs::remove_dir_all(&root);
                    continue;
                }
                fs::create_dir(&old_workspace).expect("the old Workspace root is created");
                fs::create_dir(old_workspace.join("src")).expect("the old cwd is created");
                fs::create_dir(&new_workspace).expect("the new Workspace root is created");
                fs::create_dir(new_workspace.join("src")).expect("the new cwd is created");
                set_private_directory_mode(&old_workspace);
                set_private_directory_mode(&new_workspace);
                set_private_directory_mode(&old_workspace.join("src"));
                set_private_directory_mode(&new_workspace.join("src"));
                create_marked_store(&root);
                create_fixture_agent(&root);
                create_fixture_session(&root, &old_workspace);
                return Self {
                    root,
                    old_workspace,
                    new_workspace,
                };
            }
        }

        fn session_path(&self) -> PathBuf {
            self.root.join("sessions").join(SESSION_ID)
        }

        fn next_generation_path(&self) -> PathBuf {
            self.session_path().join("generations").join(G2)
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.old_workspace);
            let _ = fs::remove_dir_all(&self.new_workspace);
        }
    }

    struct LoadedFixture {
        context: RuntimeTaskContext,
        state: DurableState,
        executor: SessionExecutor,
        definition: Arc<SessionDefinition>,
        resolver: Arc<WorkspaceResolver>,
        lifecycle_closing: CancellationToken,
    }

    fn set_private_directory_mode(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("the fixture directory receives private mode");
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn set_private_file_mode(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("the fixture file receives private mode");
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn create_file(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("the fixture file is created");
        set_private_file_mode(path);
    }

    fn create_dir(path: &Path) {
        fs::create_dir(path).expect("the fixture directory is created");
        set_private_directory_mode(path);
    }

    fn create_marked_store(root: &Path) {
        create_file(&root.join(".minicore.lock"), b"");
        create_file(&root.join("MINICORE_STORE_V1"), b"");
        create_dir(&root.join("reservations"));
        create_dir(&root.join("reservations").join("agents"));
        create_dir(&root.join("reservations").join("sessions"));
        create_dir(&root.join("agents"));
        create_dir(&root.join("sessions"));
    }

    fn workspace_uri(path: &Path) -> CanonicalFileUri {
        #[cfg(windows)]
        {
            let path = path.to_string_lossy().replace('\\', "/");
            let path = path.strip_prefix('/').unwrap_or(&path);
            return format!("file:///{path}")
                .parse()
                .expect("temporary Windows URI");
        }
        #[cfg(not(windows))]
        {
            CanonicalFileUri::from_decoded_parts(
                FileUriFamily::Posix,
                None,
                path.to_str().expect("temporary path is UTF-8"),
            )
            .expect("temporary POSIX URI")
        }
    }

    fn replace_fixture(input: &[u8], from: &str, to: &str) -> Vec<u8> {
        let input = std::str::from_utf8(input).expect("fixture bytes are UTF-8");
        assert_eq!(
            input.matches(from).count(),
            1,
            "fixture replacement is unique"
        );
        input.replacen(from, to, 1).into_bytes()
    }

    fn session_definition_fixture(workspace: &Path) -> Vec<u8> {
        let input = include_bytes!("../docs/fixtures/durable-store-v1/session-definition.json");
        replace_fixture(
            input,
            "file:///Users/example/project",
            workspace_uri(workspace).as_str(),
        )
    }

    fn create_fixture_agent(root: &Path) {
        let reservation = root.join("reservations").join("agents").join(AGENT_ID);
        create_file(&reservation, b"");
        let entity = root.join("agents").join(AGENT_ID);
        create_dir(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        let generation = entity.join("generations");
        create_dir(&generation);
        let g1 = generation.join(G1);
        create_dir(&g1);
        create_file(
            &g1.join("head.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-head.json"),
        );
        create_file(
            &g1.join("definition.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-definition.json"),
        );
        create_file(&g1.join("COMMITTED"), b"");
    }

    fn create_fixture_agent_g2(root: &Path) {
        let generation = root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G2);
        create_dir(&generation);
        create_file(
            &generation.join("head.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-head-2-definition.json"),
        );
        create_file(
            &generation.join("definition.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/agent-definition-2.json"),
        );
        create_file(&generation.join("COMMITTED"), b"");
    }

    fn conversation_header_fixture() -> Vec<u8> {
        format!(
            "{{\"type\":\"session_header\",\"data\":{{\"formatVersion\":1,\"sessionId\":\"{SESSION_ID}\",\"createdAt\":\"2026-08-03T10:01:00.456Z\",\"initialAgent\":{{\"agentId\":\"{AGENT_ID}\",\"revision\":\"ar_1\"}},\"initialDefinitionRevision\":\"sdr_1\"}}}}\n"
        )
        .into_bytes()
    }

    fn create_fixture_session(root: &Path, workspace: &Path) {
        let reservation = root.join("reservations").join("sessions").join(SESSION_ID);
        create_file(&reservation, b"");
        let entity = root.join("sessions").join(SESSION_ID);
        create_dir(&entity);
        create_file(&entity.join("PUBLISHED"), b"");
        create_file(
            &entity.join("conversation.jsonl"),
            &conversation_header_fixture(),
        );
        let generation = entity.join("generations");
        create_dir(&generation);
        let g1 = generation.join(G1);
        create_dir(&g1);
        create_file(
            &g1.join("head.json"),
            include_bytes!("../docs/fixtures/durable-store-v1/session-head.json"),
        );
        create_file(
            &g1.join("definition.json"),
            &session_definition_fixture(workspace),
        );
        create_file(&g1.join("COMMITTED"), b"");
    }

    async fn open_state(root: &Path) -> (RuntimeTaskContext, DurableState) {
        let context = RuntimeTaskContext::new(Handle::current())
            .await
            .expect("the test runtime has a time driver");
        let result = DurableState::open(root.to_owned(), context.clone()).await;
        match result {
            Ok(state) => (context, state),
            Err(error) => {
                context.shutdown().await;
                panic!("fixture Store opens: {error:?}");
            }
        }
    }

    async fn loaded_fixture(store: &TempStore) -> LoadedFixture {
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state
            .session_current_definition(session_id)
            .expect("the fixture Session definition is current");
        let candidate = resolver
            .resolve(session_id, definition.workspace())
            .await
            .expect("the fixture Workspace resolves");
        let prompts = Arc::from(Vec::new().into_boxed_slice());
        let skills = Arc::from(Vec::new().into_boxed_slice());
        let workspace_snapshot = candidate.finish(prompts, skills).unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_without_conversation(
            context.clone(),
            state.clone(),
            resolver.clone(),
            prompt_service,
            Arc::clone(&definition),
            workspace_snapshot,
        )
        .expect("the loaded Ready+Idle executor starts");
        LoadedFixture {
            context,
            state,
            executor,
            definition,
            resolver,
            lifecycle_closing: CancellationToken::new(),
        }
    }

    async fn scripted_text_fixture(
        store: &TempStore,
        model: &ScriptedModelFixture,
    ) -> LoadedFixture {
        scripted_text_fixture_with_tools(store, model, ToolSet::empty()).await
    }

    async fn scripted_text_fixture_with_compaction(
        store: &TempStore,
        model: &ScriptedModelFixture,
        compaction: CompactionSettingsSnapshot,
    ) -> LoadedFixture {
        scripted_text_fixture_with_tools_and_compaction(store, model, ToolSet::empty(), compaction)
            .await
    }

    async fn scripted_text_fixture_with_tools(
        store: &TempStore,
        model: &ScriptedModelFixture,
        tool_set: Arc<ToolSet>,
    ) -> LoadedFixture {
        scripted_text_fixture_with_tools_and_compaction(
            store,
            model,
            tool_set,
            CompactionSettings::default().validate().unwrap(),
        )
        .await
    }

    async fn scripted_text_fixture_with_tools_and_compaction(
        store: &TempStore,
        model: &ScriptedModelFixture,
        tool_set: Arc<ToolSet>,
        compaction: CompactionSettingsSnapshot,
    ) -> LoadedFixture {
        for (path, from, to) in [
            (
                store
                    .root
                    .join("agents")
                    .join(AGENT_ID)
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
            (
                store
                    .session_path()
                    .join("generations")
                    .join(G1)
                    .join("definition.json"),
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        ] {
            let bytes = fs::read(&path).expect("the fixture definition is readable");
            create_file(&path, &replace_fixture(&bytes, from, to));
        }

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state
            .session_current_definition(session_id)
            .expect("the fixture Session definition is current");
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .expect("the fixture Workspace resolves")
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let lifecycle_closing = CancellationToken::new();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources_and_lifecycle(
            SessionExecutorDependencies::with_turn_resources_and_tools_and_compaction(
                context.clone(),
                state.clone(),
                Arc::clone(&resolver),
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
                compaction,
            ),
            Arc::clone(&definition),
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
            lifecycle_closing.clone(),
        )
        .unwrap();
        LoadedFixture {
            context,
            state,
            executor,
            definition,
            resolver,
            lifecycle_closing,
        }
    }

    async fn scripted_pending_tool_interaction_fixture(
        store: &TempStore,
        model: &ScriptedModelFixture,
        request_id: RequestId,
    ) -> LoadedFixture {
        let interaction_request =
            InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture());
        let allowed = ToolExecutionResult::completed_text("tool ran").unwrap();
        let denied = ToolExecutionResult::PreExecution {
            disposition: crate::tools::ToolResultDisposition::Denied,
            content: crate::tools::ToolResultContent::from_text_parts(vec![
                "approval denied".to_owned(),
            ])
            .unwrap(),
        };
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            {
                let interaction_request = interaction_request.clone();
                let allowed = allowed.clone();
                let denied = denied.clone();
                move |_| {
                    let interaction_request = interaction_request.clone();
                    let allowed = allowed.clone();
                    let denied = denied.clone();
                    Box::pin(async move {
                        ToolExecutionResult::Interaction {
                            request_id,
                            request: interaction_request,
                            allowed: Box::new(allowed),
                            denied: Box::new(denied),
                        }
                    })
                }
            },
        );
        scripted_text_fixture_with_tools(store, model, tool_set).await
    }

    fn changed_workspace(path: &Path) -> Workspace {
        let key: WorkspaceRootKey = "repo".parse().unwrap();
        lower_workspace(
            WorkspaceDefinitionInput::new(
                WorkspaceRootInput::new(
                    key.clone(),
                    workspace_uri(path),
                    RequestedFilesystemAccess::ReadWrite,
                    WorkspaceSourcePolicy::new(true, true),
                ),
                Vec::new(),
                WorkspaceCwdSpec::new(key, "src".parse().unwrap()),
            )
            .unwrap(),
            "wr_99".parse().unwrap(),
            WorkspacePathTarget::current(),
        )
        .unwrap()
    }

    async fn close_loaded(loaded: LoadedFixture) {
        let _ = loaded.executor.close().await;
        loaded.state.close().await;
        // DurableState closes the shared owner; retaining this explicit field makes the fixture's
        // ownership visible and prevents accidental detached context tasks in future test edits.
        let _ = loaded.context;
    }

    async fn wait_for_terminal(executor: &SessionExecutor, turn_id: TurnId) -> SessionTurnTerminal {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if let Some((completed_turn, terminal)) = snapshot.last_terminal() {
                    assert_eq!(completed_turn, turn_id);
                    return terminal;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the scripted Turn reaches terminal state")
    }

    async fn wait_for_request_count(model: &ScriptedModelFixture, expected: usize) {
        for _ in 0..100_000 {
            if model.request_count() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the scripted provider did not reach the expected attempt count");
    }

    fn active_compaction_settings(max_compactions_per_turn: u8) -> CompactionSettingsSnapshot {
        CompactionSettings {
            enabled: true,
            pressure_reserve_tokens: NonZeroU32::new(32).unwrap(),
            summary_min_output_tokens: NonZeroU32::new(8).unwrap(),
            summary_max_output_tokens: NonZeroU32::new(16).unwrap(),
            minimum_reclaimed_tokens: NonZeroU32::new(32).unwrap(),
            max_compactions_per_turn: NonZeroU8::new(max_compactions_per_turn).unwrap(),
            summary_safety_reserve_tokens: NonZeroU32::new(8).unwrap(),
        }
        .validate()
        .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn proactive_compaction_replaces_live_prefix_before_agent_run() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_context_window_tokens(
            vec!["portable rolling summary", "answer after compaction"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("retained context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(requests[1].purpose(), ModelCallPurpose::AgentRun);
        assert_eq!(requests[1].input().messages().len(), 1);
        match requests[1].input().messages()[0].as_ref() {
            crate::prompt::ModelMessageRef::User { content } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].as_text(), "portable rolling summary");
            }
            _ => panic!("the next AgentRun must consume the installed rolling summary"),
        }

        let live_state = loaded.executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            assert!(live.selected_entries().iter().any(|entry| {
                matches!(entry.body(), StoredEntryBody::Compaction(compaction)
                    if compaction.summary() == "portable rolling summary")
            }));
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                2
            );
        }

        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the compacted conversation recording is readable");
        assert!(recording.contains(r#""type":"compaction""#));
        assert!(recording.contains("portable rolling summary"));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_context_overflow_compacts_and_retries_agent_run() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::ContextOverflow],
            vec![
                "overflow recovery summary",
                "answer after provider overflow",
            ],
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("provider overflow context ".repeat(20)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].purpose(), ModelCallPurpose::AgentRun);
        assert_eq!(requests[1].purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(requests[2].purpose(), ModelCallPurpose::AgentRun);
        assert_ne!(
            requests[0].source_revision(),
            requests[2].source_revision(),
            "the live Replace must advance the next AgentRun revision"
        );
        assert_eq!(requests[2].input().messages().len(), 1);
        match requests[2].input().messages()[0].as_ref() {
            crate::prompt::ModelMessageRef::User { content } => {
                assert_eq!(content[0].as_text(), "overflow recovery summary");
            }
            _ => panic!("the recovered AgentRun must consume the overflow summary"),
        }
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn compaction_summary_retries_once_with_same_request_arc() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses_and_context_window(
            vec![ModelCallErrorReason::Timeout],
            vec!["summary after retry", "answer after summary retry"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("retry compaction context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(requests[1].purpose(), ModelCallPurpose::CompactionSummary);
        assert!(Arc::ptr_eq(&requests[0], &requests[1]));
        assert_eq!(requests[2].purpose(), ModelCallPurpose::AgentRun);

        let live_state = loaded.executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            let compactions = live
                .selected_entries()
                .iter()
                .filter_map(|entry| match entry.body() {
                    StoredEntryBody::Compaction(compaction) => Some(compaction),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(compactions.len(), 1);
            assert_eq!(
                compactions[0]
                    .model_call()
                    .expect("automatic compaction keeps provenance")
                    .logical_retry_count(),
                1
            );
        }
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn compaction_summary_stops_after_one_logical_retry() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses_and_context_window(
            vec![
                ModelCallErrorReason::Timeout,
                ModelCallErrorReason::ProviderUnavailable,
            ],
            vec!["must not run"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("retry exhaustion context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert!(Arc::ptr_eq(&requests[0], &requests[1]));
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert_eq!(model.request_count(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_control_after_compaction_summary_never_applies_replace() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_context_window_tokens(
            vec!["stale summary", "must not run"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_compaction_apply();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("stale compaction context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_compaction_apply().await;
        assert_eq!(
            loaded
                .executor
                .snapshot()
                .await
                .unwrap()
                .current_turn_view()
                .expect("the compaction belongs to the running Turn")
                .phase(),
            Some(TurnExecutionPhaseView::Compacting)
        );
        loaded
            .executor
            .invalidate_control_generation_for_test(turn_id);
        hooks.release_before_compaction_apply();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert!(
            lock(&live_state)
                .selected_entries()
                .iter()
                .all(|entry| !matches!(entry.body(), StoredEntryBody::Compaction(_)))
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn security_revoke_after_compaction_summary_prevents_replace() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_context_window_tokens(
            vec!["revoked summary", "must not run"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_compaction_apply();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("revoked compaction context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_compaction_apply().await;
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(turn_id))
                .await,
            Ok(())
        );
        hooks.release_before_compaction_apply();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
        );
        assert_eq!(model.request_count(), 1);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert!(
            lock(&live_state)
                .selected_entries()
                .iter()
                .all(|entry| !matches!(entry.body(), StoredEntryBody::Compaction(_)))
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_recording_failure_keeps_live_summary_and_turn_progress() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_context_window_tokens(
            vec!["live-only summary", "answer from live summary"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_compaction_apply();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("record failure context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_compaction_apply().await;
        let barrier = RecorderWriteBarrier::new();
        loaded
            .executor
            .recorder_for_test()
            .unwrap()
            .set_write_barrier_for_test(Arc::clone(&barrier));
        barrier.fail_before_write();
        hooks.release_before_compaction_apply();
        hooks.wait_before_agent_run_attempt().await;
        let compacted_snapshot = loaded.executor.snapshot().await.unwrap();
        assert_eq!(
            compacted_snapshot.recording(),
            SessionRecordingState::Degraded
        );
        assert_eq!(
            compacted_snapshot
                .usage()
                .expect("the compacted live conversation projects usage")
                .compaction_calls(),
            1
        );
        assert_eq!(
            compacted_snapshot
                .current_turn_view()
                .expect("the next AgentRun still belongs to the running Turn")
                .phase(),
            Some(TurnExecutionPhaseView::Sampling)
        );
        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        assert_eq!(model.requests()[1].purpose(), ModelCallPurpose::AgentRun);
        match model.requests()[1].input().messages()[0].as_ref() {
            crate::prompt::ModelMessageRef::User { content } => {
                assert_eq!(content[0].as_text(), "live-only summary");
            }
            _ => panic!("recording failure must not roll back the live summary"),
        }
        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert!(lock(&live_state).selected_entries().iter().any(|entry| {
            matches!(entry.body(), StoredEntryBody::Compaction(compaction)
                if compaction.summary() == "live-only summary")
        }));
        assert!(matches!(
            &*loaded.executor.recorder_for_test().unwrap().health(),
            RecordingHealth::Degraded { .. }
        ));
        assert_eq!(
            loaded.executor.snapshot().await.unwrap().recording(),
            SessionRecordingState::Degraded
        );
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the degraded recording prefix remains readable");
        assert!(!recording.contains(r#""type":"compaction""#));
        assert!(!recording.contains("live-only summary"));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compaction_limit_counts_operations_not_summary_attempts() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_responses_then_failure_reasons(
            vec!["only allowed summary"],
            vec![ModelCallErrorReason::ContextOverflow],
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(1))
                .await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("single compaction context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::ContextOverflow)
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].purpose(), ModelCallPurpose::CompactionSummary);
        assert_eq!(requests[1].purpose(), ModelCallPurpose::AgentRun);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert_eq!(
            lock(&live_state)
                .selected_entries()
                .iter()
                .filter(|entry| matches!(entry.body(), StoredEntryBody::Compaction(_)))
                .count(),
            1
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_steer_is_applied_at_post_compaction_safe_point() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_context_window_tokens(
            vec!["summary before steer", "answer after steer"],
            4_300,
        );
        let loaded =
            scripted_text_fixture_with_compaction(&store, &model, active_compaction_settings(2))
                .await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_compaction_apply();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(
                        TextIntent::new("steer compaction context ".repeat(160)).unwrap(),
                    ),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_compaction_apply().await;
        assert_eq!(
            loaded
                .executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("focus after summary").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        hooks.release_before_compaction_apply();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].purpose(), ModelCallPurpose::AgentRun);
        assert_eq!(requests[1].input().messages().len(), 2);
        for (message, expected) in requests[1]
            .input()
            .messages()
            .iter()
            .zip(["summary before steer", "focus after summary"])
        {
            match message.as_ref() {
                crate::prompt::ModelMessageRef::User { content } => {
                    assert_eq!(content[0].as_text(), expected);
                }
                _ => panic!("post-compaction input must retain summary then Steer order"),
            }
        }
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_scripted_turn_records_and_replays_user_and_final_assistant() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::new(vec!["scripted answer"]);
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let candidate = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap();
        let workspace = candidate.finish(Arc::from([]), Arc::from([])).unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                Arc::clone(&resolver),
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            Arc::clone(&definition),
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();
        let mut subscription = executor.subscribe().await.unwrap();
        assert_eq!(
            subscription.snapshot().execution_state(),
            SessionExecutionState::Idle
        );
        let command_id = CommandId::generate().unwrap();
        let turn_id = executor
            .submit(
                command_id,
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello runtime").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.command_id(), Some(command_id));
        assert_eq!(event.turn_id(), Some(turn_id));
        assert_eq!(event.terminal(), Some(SessionTurnTerminal::Completed));
        assert_eq!(event.snapshot().current_turn(), None);
        assert_eq!(
            event.snapshot().execution_state(),
            SessionExecutionState::Idle
        );
        assert_eq!(model.request_count(), 1);
        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            assert_eq!(live.current_turn(), None);
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                2
            );
        }
        executor.close().await.unwrap();

        let replayed = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(replayed.live_state.current_turn(), None);
        assert_eq!(
            replayed
                .live_state
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            2
        );

        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_service.initialize().await.unwrap(),
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                replayed.live_state,
                replayed.recorder,
                replayed.diagnostics,
            ),
        )
        .unwrap();
        let failed_turn = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("second request").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_terminal(&executor, failed_turn).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 2);
        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            assert_eq!(live.current_turn(), None);
            assert_eq!(
                live.capture_conversation_views()
                    .unwrap()
                    .conversation()
                    .messages()
                    .len(),
                3
            );
        }
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_retries_delivery_safe_transient_with_same_request_arc() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["retry succeeded"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("retry me").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let revision_before_backoff = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert!(Arc::ptr_eq(&requests[0], &requests[1]));
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the retry result is recorded");
        assert!(recording.contains(r#""logicalRetryCount":1"#));
        let revision_after_retry = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_ne!(revision_before_backoff, revision_after_retry);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_does_not_retry_unknown_or_stream_interrupted_failures() {
        for reason in [
            ModelCallErrorReason::RequestOutcomeUnknown,
            ModelCallErrorReason::StreamInterrupted,
        ] {
            let store = TempStore::new();
            let model = ScriptedModelFixture::with_failure_reasons_then_responses(
                vec![reason],
                vec!["must not run"],
            );
            let loaded = scripted_text_fixture(&store, &model).await;
            let turn_id = loaded
                .executor
                .submit(
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("do not retry").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                wait_for_terminal(&loaded.executor, turn_id).await,
                SessionTurnTerminal::Failed(SessionTurnFailure::Model)
            );
            assert_eq!(model.request_count(), 1);
            close_loaded(loaded).await;
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn agent_run_retry_exhaustion_stops_after_four_gateway_attempts() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![
                ModelCallErrorReason::Timeout,
                ModelCallErrorReason::TransportUnavailable,
                ModelCallErrorReason::ProviderUnavailable,
                ModelCallErrorReason::Timeout,
            ],
            Vec::new(),
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("exhaust retries").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        wait_for_request_count(&model, 2).await;
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        wait_for_request_count(&model, 3).await;
        tokio::time::advance(std::time::Duration::from_secs(8)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 4);
        let requests = model.requests();
        assert_eq!(requests.len(), 4);
        assert!(
            requests
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0], &pair[1]))
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_during_agent_run_retry_backoff_sends_no_extra_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("cancel retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        wait_for_request_count(&model, 1).await;
        assert!(
            loaded
                .executor
                .cancel(
                    SessionCancelTarget::Turn(turn_id),
                    "2026-08-08T10:03:00.000Z".parse().unwrap(),
                )
                .await
                .is_ok()
        );
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::UserCancelled)
        );
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_controls_bypass_a_full_work_lane() {
        for security_revoked in [false, true] {
            let store = TempStore::new();
            let model = ScriptedModelFixture::new(vec!["must not run"]);
            let loaded = scripted_text_fixture(&store, &model).await;
            let hooks = loaded.executor.test_hooks();
            hooks.arm_before_agent_run_attempt();
            let command_id = CommandId::generate().unwrap();
            let turn_id = loaded
                .executor
                .submit(
                    command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("emergency lane").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            hooks.wait_before_agent_run_attempt().await;

            let mut permits = Vec::new();
            for _ in 0..SESSION_EXECUTOR_REQUEST_QUEUE_CAPACITY {
                permits.push(loaded.executor.sender.reserve().await.unwrap());
            }

            if security_revoked {
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    loaded
                        .executor
                        .security_revoke(SessionCancelTarget::Turn(turn_id)),
                )
                .await
                .expect("SecurityRevoked bypasses the full work lane")
                .unwrap();
            } else {
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    loaded.executor.cancel(
                        SessionCancelTarget::Turn(turn_id),
                        "2026-08-08T10:05:00.000Z".parse().unwrap(),
                    ),
                )
                .await
                .expect("Cancel bypasses the full work lane")
                .unwrap();
            }
            drop(permits);
            hooks.release_before_agent_run_attempt();
            let expected = if security_revoked {
                SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
            } else {
                SessionTurnTerminal::Interrupted(SessionTurnInterruption::UserCancelled)
            };
            assert_eq!(wait_for_terminal(&loaded.executor, turn_id).await, expected);
            assert_eq!(model.request_count(), 0);
            close_loaded(loaded).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn security_revoked_running_turn_is_sticky_and_stops_model_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("security revoke retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_agent_run_attempt().await;
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(turn_id))
                .await,
            Ok(())
        );
        assert_eq!(
            loaded.executor.snapshot().await.unwrap().execution_state(),
            SessionExecutionState::Finishing
        );
        assert_eq!(
            loaded
                .executor
                .emergency_control_for_test()
                .observe(EmergencyControlTarget::Turn(turn_id))
                .and_then(|observation| observation.signal()),
            Some(EmergencyControlSignal::SecurityRevoked)
        );
        assert_eq!(
            loaded.executor.emergency_control_for_test().signal(
                EmergencyControlTarget::Turn(turn_id),
                EmergencyControlSignal::Cancel,
            ),
            EmergencyControlSignalOutcome::AlreadySignaled {
                epoch: loaded
                    .executor
                    .emergency_control_for_test()
                    .observe(EmergencyControlTarget::Turn(turn_id))
                    .expect("the active SecurityRevoked target remains bound")
                    .epoch(),
                signal: EmergencyControlSignal::SecurityRevoked,
            }
        );
        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
        );
        assert_eq!(model.request_count(), 0);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_retry_basis_stops_before_the_next_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("stale retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let revision = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(
            loaded
                .executor
                .retry_basis_matches_for_test(turn_id, revision),
            Some(true)
        );
        assert_eq!(
            loaded
                .executor
                .retry_basis_matches_for_test(turn_id, revision.checked_next().unwrap()),
            Some(false)
        );
        loaded
            .executor
            .invalidate_control_generation_for_test(turn_id);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn executor_lifecycle_close_interrupts_retry_backoff() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let _turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("close retry").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        loaded.lifecycle_closing.cancel();
        assert!(loaded.executor.close().await.is_ok());
        assert_eq!(model.request_count(), 1);
        assert_eq!(loaded.executor.published_snapshot().current_turn(), None);
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn executor_close_after_turn_admission_still_runs_first_model_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["admitted attempt"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("close after admission").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        hooks.wait_before_agent_run_attempt().await;
        assert_eq!(model.request_count(), 0);
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        assert_eq!(model.request_count(), 0);
        hooks.release_before_agent_run_attempt();
        assert!(close.await.is_ok());
        assert_eq!(model.request_count(), 1);
        assert_eq!(
            loaded.executor.published_snapshot().last_terminal(),
            Some((turn_id, SessionTurnTerminal::Completed))
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn steer_queued_during_agent_run_retry_backoff_is_consumed_after_success() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["retry candidate", "after steer"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("initial").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        wait_for_request_count(&model, 1).await;
        let revision_before_steer = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(
            loaded
                .executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("queued steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let revision_after_steer = lock(&loaded.executor.live_state_for_test().unwrap())
            .capture_conversation_views()
            .unwrap()
            .conversation()
            .revision();
        assert_eq!(revision_before_steer, revision_after_steer);
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 3);
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("queued steer"));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_candidate_steer_wins_and_records_continue_before_steer() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["candidate", "answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_steer_safe_point();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_before_steer_safe_point().await;

        let steer_executor = loaded.executor.clone();
        let steer = tokio::spawn(async move {
            steer_executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("focus").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
        });
        assert_eq!(steer.await.unwrap(), Ok(()));
        hooks.release_before_steer_safe_point();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 4);
            assert!(matches!(
                messages[1].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { .. }
            ));
            assert!(matches!(
                messages[2].as_ref(),
                crate::prompt::ModelMessageRef::User { .. }
            ));
            assert!(matches!(
                messages[3].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { .. }
            ));
        }

        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        let intermediate = recording
            .find(r#""disposition":"intermediate""#)
            .expect("the candidate is recorded as Intermediate");
        let steer = recording
            .find(r#""source":"steer""#)
            .expect("the Steer is recorded");
        let final_assistant = recording
            .rfind(r#""disposition":"final""#)
            .expect("the final assistant is recorded");
        assert!(intermediate < steer && steer < final_assistant);

        loaded.executor.close().await.unwrap();
        loaded.state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_signal_during_final_steer_arbitration_drops_the_candidate() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["must not apply"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_steer_safe_point();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("revoke at arbitration").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_before_steer_safe_point().await;
        let emergency = loaded.executor.emergency_control_for_test();
        assert!(matches!(
            emergency.signal(
                EmergencyControlTarget::Turn(turn_id),
                EmergencyControlSignal::SecurityRevoked,
            ),
            EmergencyControlSignalOutcome::Accepted { .. }
        ));
        hooks.release_before_steer_safe_point();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert_eq!(
            lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            1,
            "the signaled candidate never enters live state"
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_signal_after_steer_resolution_drops_the_steer() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["candidate"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_steer_safe_point();
        hooks.arm_after_steer_resolution();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("resolve then revoke").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_before_steer_safe_point().await;
        assert_eq!(
            loaded
                .executor
                .steer(
                    turn_id,
                    CommandId::generate().unwrap(),
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("must not apply").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        hooks.release_before_steer_safe_point();
        hooks.wait_after_steer_resolution().await;
        let emergency = loaded.executor.emergency_control_for_test();
        assert!(matches!(
            emergency.signal(
                EmergencyControlTarget::Turn(turn_id),
                EmergencyControlSignal::SecurityRevoked,
            ),
            EmergencyControlSignalOutcome::Accepted { .. }
        ));
        hooks.release_after_steer_resolution();

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        assert!(recording.contains(r#""disposition":"intermediate""#));
        assert!(!recording.contains(r#""source":"steer""#));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_candidate_final_reservation_wins_and_closes_steer_admission() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["candidate"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_steer_arbitration();

        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("hello").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_after_steer_arbitration().await;

        let live_state = loaded.executor.live_state_for_test().unwrap();
        assert_eq!(
            lock(&live_state)
                .capture_conversation_views()
                .unwrap()
                .conversation()
                .messages()
                .len(),
            1,
            "final reservation does not mutate the live conversation"
        );

        let steer_executor = loaded.executor.clone();
        let mut steer = Box::pin(
            steer_executor.steer(
                turn_id,
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("late focus").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            ),
        );
        assert!(poll_once_pending(steer.as_mut()).await);
        hooks.release_after_steer_arbitration();
        assert_eq!(steer.await, Err(SessionSteerError::TurnNotRunning));

        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 2);
        }
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        assert_eq!(recording.matches(r#""disposition":"final""#).count(), 1);
        assert!(!recording.contains(r#""source":"steer""#));

        loaded.executor.close().await.unwrap();
        loaded.state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_scripted_turn_consumes_steer_after_a_tool_round_before_the_final_model_call()
    {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_echo",
            "echo",
            "{\"value\":1}",
            "tool complete",
        );
        let tool_started = Arc::new(tokio::sync::Notify::new());
        let release_tool = Arc::new(tokio::sync::Notify::new());
        let tool_started_for_executor = Arc::clone(&tool_started);
        let release_tool_for_executor = Arc::clone(&release_tool);
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Parallel,
                )
                .unwrap(),
            ],
            move |call| {
                let tool_started = Arc::clone(&tool_started_for_executor);
                let release_tool = Arc::clone(&release_tool_for_executor);
                Box::pin(async move {
                    tool_started.notify_one();
                    release_tool.notified().await;
                    ToolExecutionResult::completed_text(call.call().arguments().canonical_json())
                        .unwrap()
                })
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        tool_started.notified().await;
        let consumed_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    consumed_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("steer while tool runs").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let cross_lane_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cross_lane_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cross-lane steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .follow_up(
                    cross_lane_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("duplicate follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionFollowUpError::CommandConflict)
        );
        assert_eq!(
            executor
                .cancel_queued_message(cross_lane_steer_command_id)
                .await,
            Ok(())
        );
        let cross_lane_follow_up_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .follow_up(
                    cross_lane_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cross-lane follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cross_lane_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("duplicate steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionSteerError::CommandConflict)
        );
        assert_eq!(
            executor
                .cancel_queued_message(cross_lane_follow_up_command_id)
                .await,
            Ok(())
        );
        let cancelled_steer_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .steer(
                    turn_id,
                    cancelled_steer_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancelled steer").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        let cancelled_follow_up_command_id = CommandId::generate().unwrap();
        assert_eq!(
            executor
                .follow_up(
                    cancelled_follow_up_command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancelled follow-up").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_steer_command_id)
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_follow_up_command_id)
                .await,
            Ok(())
        );
        assert_eq!(
            executor
                .cancel_queued_message(cancelled_steer_command_id)
                .await,
            Err(SessionQueuedMessageError::NotQueued)
        );
        release_tool.notify_one();
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);

        let live_state = executor.live_state_for_test().unwrap();
        {
            let live = lock(&live_state);
            let captured = live.capture_conversation_views().unwrap();
            let messages = captured.conversation().messages();
            assert_eq!(messages.len(), 5);
            assert!(matches!(
                messages[1].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { content }
                    if matches!(content[0].as_ref(), crate::prompt::ModelAssistantContentRef::ToolCall { .. })
            ));
            assert!(matches!(
                messages[2].as_ref(),
                crate::prompt::ModelMessageRef::Tool { .. }
            ));
            assert!(matches!(
                messages[3].as_ref(),
                crate::prompt::ModelMessageRef::User { .. }
            ));
            assert!(matches!(
                messages[4].as_ref(),
                crate::prompt::ModelMessageRef::Assistant { content }
                    if matches!(content[0].as_ref(), crate::prompt::ModelAssistantContentRef::Text("tool complete"))
            ));
        }
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scripted_tool_interaction_is_snapshot_resolved_and_recorded_before_next_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_approval",
            "echo",
            "{\"value\":1}",
            "denied tool round complete",
        );
        let request_id: RequestId = "req_33333333333333333333333333333333".parse().unwrap();
        let interaction_request =
            InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture());
        let allowed = ToolExecutionResult::completed_text("tool ran").unwrap();
        let denied = ToolExecutionResult::PreExecution {
            disposition: crate::tools::ToolResultDisposition::Denied,
            content: crate::tools::ToolResultContent::from_text_parts(vec![
                "approval denied".to_owned(),
            ])
            .unwrap(),
        };
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            {
                let interaction_request = interaction_request.clone();
                let allowed = allowed.clone();
                let denied = denied.clone();
                move |_| {
                    let interaction_request = interaction_request.clone();
                    let allowed = allowed.clone();
                    let denied = denied.clone();
                    Box::pin(async move {
                        ToolExecutionResult::Interaction {
                            request_id,
                            request: interaction_request,
                            allowed: Box::new(allowed),
                            denied: Box::new(denied),
                        }
                    })
                }
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);
        assert_eq!(pending.public_pending_interactions().len(), 1);
        assert_eq!(pending.active_items().len(), 2);

        let resolution_key: InteractionResolutionKey =
            "irk_77777777777777777777777777777777".parse().unwrap();
        let live_state = executor.live_state_for_test().unwrap();
        lock(&live_state).script_entry_id_candidates([Err(())]);
        assert_eq!(
            executor
                .resolve_interaction(
                    turn_id,
                    *pending.pending_interactions()[0].item_id(),
                    request_id,
                    resolution_key.clone(),
                    InteractionResolutionInput::ToolApproval(
                        crate::tools::ToolApprovalDecisionInput::Deny,
                    ),
                    "2026-08-08T09:59:59.999Z".parse().unwrap(),
                )
                .await,
            Err(SessionInteractionError::InternalDispatchUnavailable)
        );
        assert_eq!(
            executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .len(),
            1
        );
        lock(&live_state).clear_scripted_entry_id_candidates();
        executor
            .resolve_interaction(
                turn_id,
                *pending.pending_interactions()[0].item_id(),
                request_id,
                resolution_key.clone(),
                InteractionResolutionInput::ToolApproval(
                    crate::tools::ToolApprovalDecisionInput::Deny,
                ),
                "2026-08-08T10:00:00.000Z".parse().unwrap(),
            )
            .await
            .unwrap();
        executor
            .resolve_interaction(
                turn_id,
                *pending.pending_interactions()[0].item_id(),
                request_id,
                resolution_key,
                InteractionResolutionInput::ToolApproval(
                    crate::tools::ToolApprovalDecisionInput::Deny,
                ),
                "2026-08-08T10:00:00.001Z".parse().unwrap(),
            )
            .await
            .expect("same logical resolution retry is idempotent");
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 2);
        assert!(
            executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .is_empty()
        );
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("pre_execution"));
        assert!(recording.contains("approval denied"));
        assert!(recording.contains("denied"));
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_pending_tool_interaction_settles_cancelled_without_a_followup_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::with_tool_round(
            "call_cancel",
            "echo",
            "{\"value\":1}",
            "must not run",
        );
        let request_id: RequestId = "req_44444444444444444444444444444444".parse().unwrap();
        let interaction_request =
            InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture());
        let allowed = ToolExecutionResult::completed_text("tool ran").unwrap();
        let denied = ToolExecutionResult::PreExecution {
            disposition: crate::tools::ToolResultDisposition::Denied,
            content: crate::tools::ToolResultContent::from_text_parts(vec![
                "approval denied".to_owned(),
            ])
            .unwrap(),
        };
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            {
                let interaction_request = interaction_request.clone();
                let allowed = allowed.clone();
                let denied = denied.clone();
                move |_| {
                    let interaction_request = interaction_request.clone();
                    let allowed = allowed.clone();
                    let denied = denied.clone();
                    Box::pin(async move {
                        ToolExecutionResult::Interaction {
                            request_id,
                            request: interaction_request,
                            allowed: Box::new(allowed),
                            denied: Box::new(denied),
                        }
                    })
                }
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);

        let timestamp: Timestamp = "2026-08-08T10:01:00.000Z".parse().unwrap();
        let mismatched_turn = TurnId::generate().unwrap();
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Turn(mismatched_turn), timestamp)
                .await,
            Err(SessionCancelError::ExpectedTurnMismatch)
        );
        assert!(
            executor
                .cancel(SessionCancelTarget::Turn(turn_id), timestamp)
                .await
                .is_ok()
        );
        assert!(
            executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .is_empty()
        );
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::UserCancelled)
        );
        assert_eq!(model.request_count(), 1);
        assert_eq!(
            executor
                .cancel(SessionCancelTarget::Turn(turn_id), timestamp)
                .await,
            Err(SessionCancelError::TurnTerminal)
        );
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("turn_cancelled"));
        assert!(recording.contains("cancelled"));
        assert!(recording.contains("pre_execution"));
        assert!(!recording.contains("must not run"));
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn security_revoke_pending_tool_interaction_settles_security_revoked_without_retry() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_tool_round(
            "call_security_revoke",
            "echo",
            "{\"value\":1}",
            "must not run",
        );
        let request_id: RequestId = "req_55555555555555555555555555555555".parse().unwrap();
        let loaded = scripted_pending_tool_interaction_fixture(&store, &model, request_id).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);

        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(turn_id))
                .await,
            Ok(())
        );
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
        );
        assert_eq!(model.request_count(), 1);
        assert!(
            loaded
                .executor
                .snapshot()
                .await
                .unwrap()
                .pending_interactions()
                .is_empty()
        );
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("security_revoked"));
        assert!(recording.contains("pre_execution"));
        assert!(recording.contains("cancelled"));
        assert!(!recording.contains("must not run"));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_pending_tool_interaction_settles_unloaded_without_followup_model_call() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_tool_round(
            "call_session_unload",
            "echo",
            "{\"value\":1}",
            "must not run",
        );
        let request_id: RequestId = "req_66666666666666666666666666666666".parse().unwrap();
        let loaded = scripted_pending_tool_interaction_fixture(&store, &model, request_id).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot.pending_interactions().len() == 1 {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Interaction request is projected");
        assert_eq!(pending.current_turn(), Some(turn_id));
        assert_eq!(pending.pending_interactions()[0].request_id(), &request_id);

        loaded.executor.close().await.unwrap();
        assert_eq!(model.request_count(), 1);
        let recording = fs::read_to_string(store.session_path().join("conversation.jsonl"))
            .expect("the scripted conversation recording is readable");
        assert!(recording.contains("interaction_requested"));
        assert!(recording.contains("interaction_resolved"));
        assert!(recording.contains("session_unloaded"));
        assert!(recording.contains("pre_execution"));
        assert!(recording.contains("cancelled"));
        assert!(!recording.contains("must not run"));
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_submit_before_input_prevents_turn_start_and_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let hooks = executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        let command_id = CommandId::generate().unwrap();
        let submit_executor = executor.clone();
        let submit = tokio::spawn(async move {
            submit_executor
                .submit(
                    command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("cancel before input").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
        });
        hooks.wait_after_agent_admission_before_input().await;
        let starting = executor.snapshot().await.unwrap();
        assert_eq!(starting.active_submit_command_id(), Some(command_id));
        let timestamp: Timestamp = "2026-08-08T10:02:00.000Z".parse().unwrap();
        assert!(
            executor
                .cancel(SessionCancelTarget::Submit(command_id), timestamp)
                .await
                .is_ok()
        );
        assert_eq!(
            executor
                .emergency_control_for_test()
                .observe(EmergencyControlTarget::Submit(command_id))
                .and_then(|observation| observation.signal()),
            Some(EmergencyControlSignal::Cancel)
        );
        hooks.release_after_agent_admission_before_input();
        assert_eq!(submit.await.unwrap(), Err(SessionSubmitError::Cancelled));
        let snapshot = executor.snapshot().await.unwrap();
        assert_eq!(snapshot.execution_state(), SessionExecutionState::Idle);
        assert_eq!(snapshot.current_turn(), None);
        assert_eq!(model.request_count(), 0);
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_in_flight_submit_joins_and_conflicting_payload_is_rejected() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["one shared model attempt"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        let command_id = CommandId::generate().unwrap();
        let intent = PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new("shared submit").unwrap()),
            Vec::new(),
        )
        .unwrap();
        let first_executor = loaded.executor.clone();
        let first_intent = intent.clone();
        let first =
            tokio::spawn(async move { first_executor.submit(command_id, first_intent).await });
        hooks.wait_after_agent_admission_before_input().await;

        let mut second = Box::pin(loaded.executor.submit(command_id, intent.clone()));
        assert!(poll_once_pending(second.as_mut()).await);
        assert_eq!(
            loaded
                .executor
                .submit(
                    command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(TextIntent::new("conflicting submit").unwrap()),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await,
            Err(SessionSubmitError::CommandConflict)
        );

        hooks.release_after_agent_admission_before_input();
        let first_turn = first.await.unwrap().unwrap();
        let second_turn = second.await.unwrap();
        assert_eq!(first_turn, second_turn);
        assert_eq!(
            wait_for_terminal(&loaded.executor, first_turn).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn security_revoke_starting_admission_is_sticky_and_prevents_turn_start() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["must not run"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_agent_admission_before_input();
        let command_id = CommandId::generate().unwrap();
        let submit_executor = loaded.executor.clone();
        let submit = tokio::spawn(async move {
            submit_executor
                .submit(
                    command_id,
                    PromptIntent::new(
                        PromptBodyIntent::Text(
                            TextIntent::new("security revoke before input").unwrap(),
                        ),
                        Vec::new(),
                    )
                    .unwrap(),
                )
                .await
        });
        hooks.wait_after_agent_admission_before_input().await;
        assert_eq!(
            loaded.executor.snapshot().await.unwrap().execution_state(),
            SessionExecutionState::Starting
        );
        let mismatched_turn: TurnId = "trn_99999999999999999999999999999999".parse().unwrap();
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(mismatched_turn))
                .await,
            Err(SessionSecurityRevokedError::ExpectedTurnMismatch)
        );
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Submit(command_id))
                .await,
            Ok(())
        );
        assert_eq!(
            loaded.executor.snapshot().await.unwrap().execution_state(),
            SessionExecutionState::Starting
        );
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Submit(command_id))
                .await,
            Err(SessionSecurityRevokedError::AlreadyRevoked)
        );
        assert_eq!(
            loaded
                .executor
                .emergency_control_for_test()
                .observe(EmergencyControlTarget::Submit(command_id))
                .and_then(|observation| observation.signal()),
            Some(EmergencyControlSignal::SecurityRevoked)
        );
        hooks.release_after_agent_admission_before_input();
        assert_eq!(
            submit.await.unwrap(),
            Err(SessionSubmitError::SecurityRevoked)
        );
        let snapshot = loaded.executor.snapshot().await.unwrap();
        assert_eq!(snapshot.execution_state(), SessionExecutionState::Idle);
        assert_eq!(snapshot.current_turn(), None);
        assert_eq!(model.request_count(), 0);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn security_revoke_reports_not_running_and_closing_without_a_target() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let turn_id: TurnId = "trn_88888888888888888888888888888888".parse().unwrap();
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(turn_id))
                .await,
            Err(SessionSecurityRevokedError::NotRunning)
        );
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Submit(CommandId::generate().unwrap(),))
                .await,
            Err(SessionSecurityRevokedError::NotRunning)
        );
        loaded.executor.request_closing();
        assert_eq!(
            loaded
                .executor
                .security_revoke(SessionCancelTarget::Turn(turn_id))
                .await,
            Err(SessionSecurityRevokedError::Closing)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emergency_after_input_is_reported_as_turn_interruption() {
        for security_revoked in [false, true] {
            let store = TempStore::new();
            let model = ScriptedModelFixture::new(vec!["must not run"]);
            let loaded = scripted_text_fixture(&store, &model).await;
            let hooks = loaded.executor.test_hooks();
            hooks.arm_after_input_before_completion();
            let command_id = CommandId::generate().unwrap();
            let submit_executor = loaded.executor.clone();
            let submit = tokio::spawn(async move {
                submit_executor
                    .submit(
                        command_id,
                        PromptIntent::new(
                            PromptBodyIntent::Text(
                                TextIntent::new("emergency after input").unwrap(),
                            ),
                            Vec::new(),
                        )
                        .unwrap(),
                    )
                    .await
            });
            hooks.wait_after_input_before_completion().await;

            if security_revoked {
                assert_eq!(
                    loaded
                        .executor
                        .security_revoke(SessionCancelTarget::Submit(command_id))
                        .await,
                    Ok(())
                );
                assert_eq!(
                    loaded.executor.snapshot().await.unwrap().execution_state(),
                    SessionExecutionState::Finishing
                );
                let live_turn = lock(&loaded.executor.live_state_for_test().unwrap())
                    .current_turn()
                    .expect("Input was applied before SecurityRevoked");
                let snapshot = loaded.executor.snapshot().await.unwrap();
                assert_eq!(snapshot.current_turn(), Some(live_turn));
                assert_eq!(snapshot.active_submit_command_id(), None);
            } else {
                let accepted = loaded
                    .executor
                    .cancel(
                        SessionCancelTarget::Submit(command_id),
                        "2026-08-08T10:04:00.000Z".parse().unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    loaded.executor.snapshot().await.unwrap().execution_state(),
                    SessionExecutionState::Finishing
                );
                assert_eq!(
                    loaded
                        .executor
                        .cancel(
                            SessionCancelTarget::Submit(command_id),
                            "2026-08-08T10:04:00.000Z".parse().unwrap(),
                        )
                        .await,
                    Ok(accepted)
                );
            }
            hooks.release_after_input_before_completion();

            let turn_id = submit.await.unwrap().unwrap();
            let expected = if security_revoked {
                SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
            } else {
                SessionTurnTerminal::Interrupted(SessionTurnInterruption::UserCancelled)
            };
            assert_eq!(wait_for_terminal(&loaded.executor, turn_id).await, expected);
            assert_eq!(model.request_count(), 0);
            close_loaded(loaded).await;
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_emergency_epoch_stops_before_the_next_attempt() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("stale emergency").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let emergency = loaded.executor.emergency_control_for_test();
        let observation = emergency
            .observe(EmergencyControlTarget::Turn(turn_id))
            .expect("the active Turn owns the emergency target");
        assert!(emergency.retire(observation));
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn emergency_signal_wakes_retry_backoff_without_waiting_for_timer() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::with_failure_reasons_then_responses(
            vec![ModelCallErrorReason::Timeout],
            vec!["must not run"],
        );
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("wake emergency").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();

        wait_for_request_count(&model, 1).await;
        let emergency = loaded.executor.emergency_control_for_test();
        assert_eq!(
            emergency.signal(
                EmergencyControlTarget::Turn(turn_id),
                EmergencyControlSignal::SecurityRevoked,
            ),
            EmergencyControlSignalOutcome::Accepted {
                epoch: emergency
                    .observe(EmergencyControlTarget::Turn(turn_id))
                    .expect("the active Turn owns the emergency target")
                    .epoch(),
            }
        );
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Interrupted(SessionTurnInterruption::SecurityRevoked)
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[test]
    fn tool_round_emergency_safe_point_rejects_signaled_or_stale_basis() {
        let emergency = EmergencyControlHandle::new();
        let target =
            EmergencyControlTarget::Turn("trn_77777777777777777777777777777777".parse().unwrap());
        let observation = emergency.bind(target).unwrap();
        assert!(emergency_control_is_unsignaled_current(
            &emergency,
            observation
        ));
        assert!(matches!(
            emergency.signal(target, EmergencyControlSignal::SecurityRevoked),
            EmergencyControlSignalOutcome::Accepted { .. }
        ));
        assert!(!emergency_control_is_unsignaled_current(
            &emergency,
            observation
        ));
        assert!(emergency.retire(observation));
        assert!(!emergency_control_is_unsignaled_current(
            &emergency,
            observation
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_tool_settlement_ends_the_turn_without_a_followup_model_call() {
        let store = TempStore::new();
        let agent_definition_path = store
            .root
            .join("agents")
            .join(AGENT_ID)
            .join("generations")
            .join(G1)
            .join("definition.json");
        let agent_definition = fs::read(&agent_definition_path).unwrap();
        create_file(
            &agent_definition_path,
            &replace_fixture(
                &agent_definition,
                r#""promptIds":["base","safety"]"#,
                r#""promptIds":[]"#,
            ),
        );
        let session_definition_path = store
            .session_path()
            .join("generations")
            .join(G1)
            .join("definition.json");
        let session_definition = fs::read(&session_definition_path).unwrap();
        create_file(
            &session_definition_path,
            &replace_fixture(
                &session_definition,
                r#""promptIds":["base","session-notes"]"#,
                r#""promptIds":[]"#,
            ),
        );

        let (context, state) = open_state(&store.root).await;
        let resolver = Arc::new(WorkspaceResolver::new(context.clone()));
        let prompt_service = Arc::new(
            PromptService::new(Arc::from("required"), None, Vec::new(), Vec::new()).unwrap(),
        );
        let prompt_resources = prompt_service.initialize().await.unwrap();
        let model =
            ScriptedModelFixture::with_tool_round("call_abandoned", "echo", "{}", "must not run");
        let tool_set = ToolSet::with_executor(
            vec![
                crate::tools::ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON value",
                    "{}".parse().unwrap(),
                    crate::tools::ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            |_| {
                Box::pin(async {
                    ToolExecutionResult::Abandoned {
                        reason: crate::tools::ToolAbandonReason::OutcomeUnknown,
                    }
                })
            },
        );
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let definition = state.session_current_definition(session_id).unwrap();
        let workspace = resolver
            .resolve(session_id, definition.workspace())
            .await
            .unwrap()
            .finish(Arc::from([]), Arc::from([]))
            .unwrap();
        let loaded = load_replayed_conversation_with_barrier_for_test(
            state.open_conversation_target(session_id).await.unwrap(),
            context.clone(),
            None,
        )
        .await
        .unwrap();
        let executor = SessionExecutor::start_loaded_ready_idle_with_turn_resources(
            SessionExecutorDependencies::with_turn_resources_and_tools(
                context.clone(),
                state.clone(),
                resolver,
                Arc::clone(&prompt_service),
                prompt_resources,
                Arc::clone(model.gateway()),
                Arc::clone(model.catalog()),
                tool_set,
            ),
            definition,
            workspace,
            LoadedSessionConversation::from_replay(
                loaded.live_state,
                loaded.recorder,
                loaded.diagnostics,
            ),
        )
        .unwrap();

        let turn_id = executor
            .submit(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("run echo").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wait_for_terminal(&executor, turn_id).await,
            SessionTurnTerminal::Failed(SessionTurnFailure::Model)
        );
        assert_eq!(model.request_count(), 1);
        let live_state = executor.live_state_for_test().unwrap();
        assert_eq!(lock(&live_state).current_turn(), None);
        let recording =
            fs::read_to_string(store.session_path().join("conversation.jsonl")).unwrap();
        assert!(recording.contains("abandoned"));
        assert!(!recording.contains("must not run"));
        executor.close().await.unwrap();
        state.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn start_ready_idle_snapshot_and_debug_are_redacted() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let snapshot = loaded.executor.snapshot().await.unwrap();
        assert_eq!(snapshot.execution_state(), SessionExecutionState::Idle);
        assert_eq!(snapshot.definition_revision().get(), 1);
        assert_eq!(snapshot.workspace_revision().get(), 1);
        assert!(Arc::ptr_eq(snapshot.definition(), &loaded.definition));
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains(SESSION_ID));
        assert!(!debug.contains(store.old_workspace.to_string_lossy().as_ref()));
        assert!(!debug.contains("2026-08-03"));
        assert!(
            !format!(
                "{:?}",
                SessionWorkspaceDefinitionError::WorkspaceUnavailable
            )
            .contains(SESSION_ID)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn follow_up_is_rejected_without_an_active_turn() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let result = loaded
            .executor
            .follow_up(
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("queued later").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(result, Err(SessionFollowUpError::TurnNotRunning));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn steer_is_rejected_without_an_active_turn() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let turn_id: TurnId = "trn_11111111111111111111111111111111".parse().unwrap();
        let result = loaded
            .executor
            .steer(
                turn_id,
                CommandId::generate().unwrap(),
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("queued steer").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(result, Err(SessionSteerError::TurnNotRunning));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_message_cancel_reports_not_queued_and_closing() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let command_id: CommandId = "cmd_33333333333333333333333333333333".parse().unwrap();
        assert_eq!(
            loaded.executor.cancel_queued_message(command_id).await,
            Err(SessionQueuedMessageError::NotQueued)
        );

        loaded.executor.request_closing();
        assert_eq!(
            loaded.executor.cancel_queued_message(command_id).await,
            Err(SessionQueuedMessageError::Closing)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_projects_queued_message_ids_and_clears_consumed_lanes() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["initial", "follow-up"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let initial_command = CommandId::generate().unwrap();
        let initial_turn = loaded
            .executor
            .submit(
                initial_command,
                PromptIntent::new(
                    PromptBodyIntent::Text(TextIntent::new("initial").unwrap()),
                    Vec::new(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        hooks.wait_before_agent_run_attempt().await;

        let follow_first = CommandId::generate().unwrap();
        let follow_second = CommandId::generate().unwrap();
        let steer_first = CommandId::generate().unwrap();
        let steer_second = CommandId::generate().unwrap();
        let follow_intent = || {
            PromptIntent::new(
                PromptBodyIntent::Text(TextIntent::new("follow-up").unwrap()),
                Vec::new(),
            )
            .unwrap()
        };
        let steer_intent = || {
            PromptIntent::new(
                PromptBodyIntent::Text(TextIntent::new("steer").unwrap()),
                Vec::new(),
            )
            .unwrap()
        };
        assert_eq!(
            loaded
                .executor
                .follow_up(follow_first, follow_intent())
                .await,
            Ok(())
        );
        assert_eq!(
            loaded
                .executor
                .follow_up(follow_second, follow_intent())
                .await,
            Ok(())
        );
        assert_eq!(
            loaded
                .executor
                .steer(initial_turn, steer_first, steer_intent())
                .await,
            Ok(())
        );
        assert_eq!(
            loaded
                .executor
                .steer(initial_turn, steer_second, steer_intent())
                .await,
            Ok(())
        );

        let queued = loaded.executor.snapshot().await.unwrap();
        assert_eq!(queued.active_submit_command_id(), None);
        assert_eq!(
            queued.follow_up_command_ids(),
            &[follow_first, follow_second]
        );
        assert_eq!(queued.steer_command_ids(), &[steer_first, steer_second]);

        assert_eq!(
            loaded.executor.cancel_queued_message(follow_second).await,
            Ok(())
        );
        assert_eq!(
            loaded.executor.cancel_queued_message(steer_second).await,
            Ok(())
        );
        let after_cancel = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after_cancel.follow_up_command_ids(), &[follow_first]);
        assert_eq!(after_cancel.steer_command_ids(), &[steer_first]);

        assert_eq!(
            loaded.executor.cancel_queued_message(steer_first).await,
            Ok(())
        );
        hooks.release_before_agent_run_attempt();
        wait_for_request_count(&model, 2).await;
        let follow_up_turn = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot
                    .current_turn()
                    .is_some_and(|turn| turn != initial_turn)
                {
                    assert!(snapshot.follow_up_command_ids().is_empty());
                    assert!(snapshot.steer_command_ids().is_empty());
                    break snapshot.current_turn().unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued FollowUp starts after the initial Turn");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot.last_terminal().is_some_and(|(turn, terminal)| {
                    turn == follow_up_turn && terminal == SessionTurnTerminal::Completed
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the queued FollowUp reaches terminal state");
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn changed_workspace_publishes_store_generation_and_reopens() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let current = loaded.executor.snapshot().await.unwrap();
        let result = loaded
            .executor
            .update_workspace_definition(
                current.definition_revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .expect("the changed Workspace publishes");
        assert!(result.changed());
        assert_eq!(result.definition_revision().get(), 2);
        assert_eq!(result.workspace_revision().get(), 2);
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        assert_eq!(after.workspace().session_id(), SESSION_ID.parse().unwrap());
        assert_eq!(after.workspace().revision().get(), 2);
        assert!(
            store
                .next_generation_path()
                .join("definition.json")
                .is_file()
        );
        assert!(
            loaded
                .state
                .session_current_definition(SESSION_ID.parse().unwrap())
                .unwrap()
                .workspace()
                .primary_root()
                .path()
                == store.new_workspace.as_path()
        );
        let conversation = store.session_path().join("conversation.jsonl");
        assert_eq!(
            fs::read(conversation).unwrap(),
            conversation_header_fixture()
        );
        close_loaded(loaded).await;

        let (context, reopened) = open_state(&store.root).await;
        let definition = reopened
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(definition.revision().get(), 2);
        assert_eq!(definition.workspace().revision().get(), 2);
        assert_eq!(
            definition.workspace().primary_root().path(),
            store.new_workspace
        );
        reopened.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publication_barrier_keeps_old_snapshot_and_admission_busy() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let publication_old = Arc::clone(&old);
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        let visible = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&visible, &old));
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        assert_eq!(
            loaded.executor.starting_admission_probe_for_test().await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        hooks.release_after_candidate_snapshot_finish_before_durable();
        assert!(publication.await.unwrap().unwrap().changed());
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_update_waiter_does_not_cancel_publication_or_install() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication_old = Arc::clone(&old);
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());
        hooks.release_after_candidate_snapshot_finish_before_durable();
        hooks.wait_for_publication_settlement().await;
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_update_waiter_after_commit_does_not_cancel_publication_or_install() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_commit_before_install();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication_old = Arc::clone(&old);
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    publication_old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks.wait_after_commit_before_install().await;
        let visible = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&visible, &old));
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());
        hooks.release_after_commit_before_install();
        hooks.wait_for_publication_settlement().await;
        let after = loaded.executor.snapshot().await.unwrap();
        assert_eq!(after.definition_revision().get(), 2);
        assert_eq!(after.workspace_revision().get(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_a_blocked_admitted_publication() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        close.await.expect("the executor closes normally");
        assert!(publication.await.unwrap().unwrap().changed());
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_a_preclose_reserved_request_permit() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let permit = loaded
            .executor
            .sender
            .clone()
            .reserve_owned()
            .await
            .expect("the open executor reserves bounded request capacity");
        let (response, waiter) = oneshot::channel();
        let request = SessionExecutorRequest::Snapshot(SnapshotRequest {
            response: Some(response),
        });

        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        // Let the actor run its close transition. The reserved permit keeps the closed receiver
        // from yielding None, so close must remain pending until this permit is consumed.
        tokio::task::yield_now().await;
        assert!(poll_once_pending(close.as_mut()).await);

        let _sender = permit.send(request);
        assert!(matches!(
            waiter.await.unwrap(),
            Err(SessionExecutorSnapshotError::Closing)
        ));
        close.await.expect("the executor closes normally");

        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_finishes_after_a_preclose_reserved_request_permit_is_dropped() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let permit = loaded
            .executor
            .sender
            .clone()
            .reserve_owned()
            .await
            .expect("the open executor reserves bounded request capacity");

        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        tokio::task::yield_now().await;
        assert!(poll_once_pending(close.as_mut()).await);
        drop(permit);
        close.await.expect("the executor closes normally");

        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_post_commit_install_and_publication_settlement() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let durable_task_count = loaded.context.registered_task_count_for_test() - 1;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_commit_before_install();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks.wait_after_commit_before_install().await;
        let mut close = Box::pin(loaded.executor.close());
        assert!(poll_once_pending(close.as_mut()).await);
        hooks.release_after_commit_before_install();
        close.await.expect("the executor closes normally");
        assert!(publication.await.unwrap().unwrap().changed());
        hooks.wait_for_publication_settlement().await;
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            durable_task_count
        );
        loaded.state.close().await;
        assert_eq!(loaded.context.registered_task_count_for_test(), 0);
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_request_lane_drains_active_publication_without_global_poison() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        publication.abort();
        assert!(publication.await.unwrap_err().is_cancelled());

        let LoadedFixture {
            context,
            state,
            executor,
            ..
        } = loaded;
        drop(executor);
        hooks.release_after_candidate_snapshot_finish_before_durable();
        hooks.wait_for_publication_settlement().await;
        assert!(!context.is_closing());
        assert_eq!(
            state
                .session_current_definition(SESSION_ID.parse().unwrap())
                .unwrap()
                .revision()
                .get(),
            2
        );
        state.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_errors_keep_old_definition_and_snapshot() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let before = loaded.executor.snapshot().await.unwrap();
        let missing = store.root.join("missing-workspace");
        let unavailable = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&missing),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            unavailable,
            Err(SessionWorkspaceDefinitionError::WorkspaceUnavailable)
        );
        let file_path = store.root.join("not-a-directory");
        create_file(&file_path, b"fixture");
        let rejected = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&file_path),
                "2026-08-03T10:03:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            rejected,
            Err(SessionWorkspaceDefinitionError::WorkspaceRejected)
        );
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after, &before));
        assert_eq!(loaded.definition.revision().get(), 1);
        assert_eq!(
            loaded
                .state
                .session_head(SESSION_ID.parse().unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canonical_noop_is_zero_resolver_io_and_stale_wins_before_noop() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let before = loaded.executor.snapshot().await.unwrap();
        fs::remove_dir_all(&store.old_workspace).unwrap();
        let no_change = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                loaded.definition.workspace().clone(),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await
            .unwrap();
        assert!(!no_change.changed());
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after, &before));
        assert!(!store.next_generation_path().exists());
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    "sdr_2".parse().unwrap(),
                    loaded.definition.workspace().clone(),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::StaleRevision)
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn aborting_publication_worker_before_durable_settlement_closes_admission() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let old = loaded.executor.snapshot().await.unwrap();
        let new_workspace = store.new_workspace.clone();
        let executor = loaded.executor.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    old.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        loaded.context.abort_latest_registered_task();
        assert_eq!(
            publication.await.unwrap(),
            Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
        );
        assert!(matches!(
            loaded.executor.snapshot().await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        assert_eq!(
            loaded.executor.close().await,
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unexpected_actor_exit_closes_owners_and_settles_future_requests() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        loaded.context.abort_latest_registered_task();
        let mut result = Box::pin(loaded.executor.snapshot());
        assert!(poll_once_pending(result.as_mut()).await);
        assert!(matches!(
            result.await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        assert_eq!(
            loaded.executor.close().await,
            Err(SessionExecutorCloseError::InternalDispatchUnavailable)
        );
        loaded.state.close().await;
        let _ = loaded.context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_commit_snapshot_install_failure_closes_admission_but_durable_reopens_new() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        loaded
            .executor
            .test_hooks()
            .fail_next_snapshot_install_after_commit();
        let before = loaded.executor.snapshot().await.unwrap();
        let result = loaded
            .executor
            .update_workspace_definition(
                before.definition_revision(),
                changed_workspace(&store.new_workspace),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
            )
            .await;
        assert_eq!(
            result,
            Err(SessionWorkspaceDefinitionError::InternalDispatchUnavailable)
        );
        assert!(matches!(
            loaded.executor.snapshot().await,
            Err(SessionExecutorSnapshotError::Closing)
        ));
        let durable = loaded
            .state
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(durable.workspace().revision().get(), 2);
        close_loaded(loaded).await;
        let (context, reopened) = open_state(&store.root).await;
        let durable = reopened
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(
            durable.workspace().primary_root().path(),
            store.new_workspace
        );
        reopened.close().await;
        let _ = context;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publication_workers_are_reaped_after_success_and_ordinary_error() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let known_durable_tasks = loaded.context.registered_task_count_for_test() - 1;

        let before_error = loaded.executor.snapshot().await.unwrap();
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    before_error.definition_revision(),
                    changed_workspace(&store.root.join("missing-workspace")),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::WorkspaceUnavailable)
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        let before_success = loaded.executor.snapshot().await.unwrap();
        assert!(
            loaded
                .executor
                .update_workspace_definition(
                    before_success.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                )
                .await
                .unwrap()
                .changed()
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        let before_second_success = loaded.executor.snapshot().await.unwrap();
        assert!(
            loaded
                .executor
                .update_workspace_definition(
                    before_second_success.definition_revision(),
                    changed_workspace(&store.old_workspace),
                    "2026-08-03T10:04:00.000Z".parse().unwrap(),
                )
                .await
                .unwrap()
                .changed()
        );
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks + 1
        );

        loaded
            .executor
            .close()
            .await
            .expect("the executor closes normally");
        assert_eq!(
            loaded.context.registered_task_count_for_test(),
            known_durable_tasks
        );
        loaded.state.close().await;
        assert_eq!(loaded.context.registered_task_count_for_test(), 0);
        let _ = loaded.context;
    }

    fn text_intent(text: &str) -> PromptIntent {
        PromptIntent::new(
            PromptBodyIntent::Text(TextIntent::new(text).unwrap()),
            Vec::new(),
        )
        .unwrap()
    }

    fn unknown_model_config() -> SessionModelConfig {
        SessionModelConfig::new(
            ModelSelection::new("openai".parse().unwrap(), "gpt-99".parse().unwrap()),
            ReasoningPreference::High,
            None,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn future_only_model_update_during_running_preserves_turn_and_changes_next_admission() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["first answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let session_id: SessionId = SESSION_ID.parse().unwrap();
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the first Turn starts");
        hooks.wait_before_agent_run_attempt().await;

        let before = loaded.executor.snapshot().await.unwrap();
        assert_eq!(before.definition_revision().get(), 1);
        assert_eq!(before.execution_state(), SessionExecutionState::Running);

        let update = loaded
            .executor
            .update_session_definition_with_cancellation(
                before.definition_revision(),
                None,
                Some(unknown_model_config()),
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await
            .expect("a future-only definition update succeeds during Running");
        assert!(update.changed());
        assert_eq!(update.definition_revision().get(), 2);
        assert_eq!(update.workspace_revision().get(), 1);

        // The installed snapshot preserves the exact WorkspaceSnapshot, current Turn, and
        // execution state while carrying the new definition revision.
        let during = loaded.executor.snapshot().await.unwrap();
        assert_eq!(during.definition_revision().get(), 2);
        assert!(Arc::ptr_eq(during.workspace(), before.workspace()));
        assert_eq!(during.workspace_revision().get(), 1);
        assert_eq!(during.current_turn(), Some(turn_id));
        assert_eq!(during.execution_state(), SessionExecutionState::Running);
        let durable = loaded.state.session_current_definition(session_id).unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(durable.model().selection().model_id().as_str(), "gpt-99");

        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        // The active Turn ran to completion against its already-captured old model.
        assert_eq!(model.request_count(), 1);

        // A future admission reads the new current definition: the replaced model is not in the
        // catalog, so the next Turn cannot capture it.
        assert_eq!(
            loaded
                .executor
                .submit(CommandId::generate().unwrap(), text_intent("second"))
                .await,
            Err(SessionSubmitError::DependencyUnavailable)
        );
        let settled = loaded.executor.snapshot().await.unwrap();
        assert_eq!(settled.execution_state(), SessionExecutionState::Idle);
        assert_eq!(settled.definition_revision().get(), 2);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workspace_change_during_running_is_busy_but_stale_still_wins() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["first answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the Turn starts");
        hooks.wait_before_agent_run_attempt().await;
        let running = loaded.executor.snapshot().await.unwrap();
        assert_eq!(running.execution_state(), SessionExecutionState::Running);

        // A true Workspace semantic change is only accepted while Idle.
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    running.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        // Stale beats busy for a changed-Workspace request during Running.
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    "sdr_99".parse().unwrap(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::StaleRevision)
        );
        let unchanged = loaded.executor.snapshot().await.unwrap();
        assert_eq!(unchanged.definition_revision().get(), 1);

        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn definition_update_never_touches_conversation_bytes_or_recorder_health() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["first answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the Turn starts");
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        let conversation_path = store.session_path().join("conversation.jsonl");
        let after_turn = fs::read(&conversation_path).unwrap();
        assert!(after_turn.len() > conversation_header_fixture().len());
        let recorder = loaded.executor.recorder_for_test().unwrap();
        assert!(matches!(&*recorder.health(), RecordingHealth::Healthy));

        let before = loaded.executor.snapshot().await.unwrap();
        let updated = loaded
            .executor
            .update_session_definition_with_cancellation(
                before.definition_revision(),
                None,
                Some(unknown_model_config()),
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await
            .expect("the Idle future-only update publishes");
        assert!(updated.changed());
        assert_eq!(fs::read(&conversation_path).unwrap(), after_turn);
        assert!(matches!(&*recorder.health(), RecordingHealth::Healthy));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_follow_up_survives_terminal_publication_race_and_starts_after_settlement() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["first answer", "second answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        let follow_up_id = CommandId::generate().unwrap();

        // Hold the future-only publication between its durable commit and its snapshot install
        // so the first Turn terminal observes an active publication.
        hooks.arm_after_commit_before_install();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the first Turn starts");
        hooks.wait_before_agent_run_attempt().await;
        let running = loaded.executor.snapshot().await.unwrap();
        assert_eq!(running.definition_revision().get(), 1);
        let publication = {
            let executor = loaded.executor.clone();
            tokio::spawn(async move {
                executor
                    .update_session_definition_with_cancellation(
                        running.definition_revision(),
                        None,
                        Some(unknown_model_config()),
                        None,
                        "2026-08-03T10:02:00.000Z".parse().unwrap(),
                        CommandId::generate().unwrap(),
                        CancellationToken::new(),
                    )
                    .await
            })
        };
        hooks.wait_after_commit_before_install().await;
        loaded
            .executor
            .follow_up(follow_up_id, text_intent("queued follow up"))
            .await
            .expect("the FollowUp is queued while the Turn runs");
        let queued = loaded.executor.snapshot().await.unwrap();
        assert_eq!(queued.follow_up_command_ids(), [follow_up_id]);

        // The Turn terminates while the publication is still active; the FollowUp stays queued.
        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        let terminal = loaded.executor.snapshot().await.unwrap();
        assert_eq!(terminal.follow_up_command_ids(), [follow_up_id]);

        // Once the publication settles, the queued FollowUp starts against the new current
        // definition: it is popped and its admission captures the replaced model, which is not
        // in the catalog.
        hooks.release_after_commit_before_install();
        assert!(publication.await.unwrap().unwrap().changed());
        assert_eq!(
            loaded
                .executor
                .snapshot()
                .await
                .unwrap()
                .definition_revision()
                .get(),
            2
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot.follow_up_command_ids().is_empty()
                    && snapshot.execution_state() == SessionExecutionState::Idle
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the FollowUp leaves the queue after the publication settles");
        // The FollowUp admission consumed the updated definition and failed capture; the first
        // Turn's single model request is unchanged.
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_while_loaded_idle_pins_revision_and_preserves_workspace_snapshot() {
        let store = TempStore::new();
        create_fixture_agent_g2(&store.root);
        let loaded = loaded_fixture(&store).await;
        let mut subscription = loaded.executor.subscribe().await.unwrap();
        let before = loaded.executor.snapshot().await.unwrap();
        assert_eq!(before.definition_revision().get(), 1);
        assert_eq!(before.definition().agent().revision().get(), 1);

        let command_id = CommandId::generate().unwrap();
        let owner_timestamp = "2026-08-03T10:02:00.000Z".parse().unwrap();
        let upgraded = loaded
            .executor
            .upgrade_session_agent_with_cancellation(
                before.definition_revision(),
                None,
                owner_timestamp,
                command_id,
                CancellationToken::new(),
            )
            .await
            .expect("the loaded Idle Agent upgrade publishes");
        assert!(upgraded.changed());
        assert_eq!(upgraded.definition_revision().get(), 2);
        assert_eq!(upgraded.workspace_revision().get(), 1);

        // The exact post-install snapshot keeps the same WorkspaceSnapshot and every non-Agent
        // durable fact while carrying the checked successor revision at the new Agent ref.
        let during = loaded.executor.snapshot().await.unwrap();
        assert_eq!(during.definition_revision().get(), 2);
        assert_eq!(during.definition().agent().revision().get(), 2);
        assert_eq!(
            during.definition().agent().agent_id(),
            before.definition().agent().agent_id()
        );
        assert!(Arc::ptr_eq(during.workspace(), before.workspace()));
        assert_eq!(during.workspace_revision().get(), 1);
        assert_eq!(
            during.definition().workspace(),
            before.definition().workspace()
        );
        assert_eq!(during.definition().model(), before.definition().model());
        assert_eq!(during.definition().prompts(), before.definition().prompts());
        assert_eq!(during.definition().created_at(), owner_timestamp);
        let durable = loaded
            .state
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(durable.agent().revision().get(), 2);

        // The exact existing DefinitionUpdated executor event carries the owner command id and
        // timestamp with the post-install snapshot.
        let event = subscription
            .recv()
            .await
            .expect("the Agent upgrade publishes one executor event");
        match event.as_ref() {
            SessionExecutorEvent::DefinitionUpdated {
                timestamp,
                command_id: event_command_id,
                snapshot,
            } => {
                assert_eq!(*timestamp, owner_timestamp);
                assert_eq!(*event_command_id, command_id);
                assert_eq!(snapshot.definition_revision().get(), 2);
                assert_eq!(snapshot.definition().agent().revision().get(), 2);
            }
            _ => panic!("the Agent upgrade publishes exactly one DefinitionUpdated event"),
        }
        // No Recorder call and no conversation write.
        assert_eq!(
            fs::read(store.session_path().join("conversation.jsonl")).unwrap(),
            conversation_header_fixture()
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_same_pin_is_nochange_without_event_or_install() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let mut subscription = loaded.executor.subscribe().await.unwrap();
        let before = loaded.executor.snapshot().await.unwrap();
        assert_eq!(before.definition().agent().revision().get(), 1);

        let noop = loaded
            .executor
            .upgrade_session_agent_with_cancellation(
                before.definition_revision(),
                Some(AgentRevisionRef::new(
                    before.definition().agent().agent_id(),
                    "ar_1".parse().unwrap(),
                )),
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await
            .expect("the same-pin upgrade returns a typed outcome");
        assert!(!noop.changed());
        assert_eq!(noop.definition_revision().get(), 1);
        assert_eq!(noop.workspace_revision().get(), 1);

        // A canonical no-op installs nothing: the same immutable snapshot Arc stays published.
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after, &before), "a no-op installs no snapshot");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), subscription.recv())
                .await
                .is_err(),
            "a no-op publishes no executor event"
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_stale_wins_and_rejects_wrong_and_unavailable_targets() {
        let store = TempStore::new();
        create_fixture_agent_g2(&store.root);
        let loaded = loaded_fixture(&store).await;
        let before = loaded.executor.snapshot().await.unwrap();
        let owner_timestamp = "2026-08-03T10:02:00.000Z".parse().unwrap();

        // Stale beats an exact same-pin no-op.
        assert_eq!(
            loaded
                .executor
                .upgrade_session_agent_with_cancellation(
                    "sdr_99".parse().unwrap(),
                    Some(before.definition().agent()),
                    owner_timestamp,
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::StaleRevision)
        );

        // A target pinned to another Agent is rejected.
        assert_eq!(
            loaded
                .executor
                .upgrade_session_agent_with_cancellation(
                    before.definition_revision(),
                    Some(AgentRevisionRef::new(
                        "agt_22222222222222222222222222222222".parse().unwrap(),
                        "ar_1".parse().unwrap(),
                    )),
                    owner_timestamp,
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::AgentMismatch)
        );

        // A future or non-retained revision is unavailable.
        assert_eq!(
            loaded
                .executor
                .upgrade_session_agent_with_cancellation(
                    before.definition_revision(),
                    Some(AgentRevisionRef::new(
                        before.definition().agent().agent_id(),
                        "ar_99".parse().unwrap(),
                    )),
                    owner_timestamp,
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::RevisionUnavailable)
        );

        // No rejected request changed the installed definition or emitted an event.
        let unchanged = loaded.executor.snapshot().await.unwrap();
        assert_eq!(unchanged.definition_revision().get(), 1);
        assert_eq!(unchanged.definition().agent().revision().get(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_during_running_preserves_active_turn_and_next_admission_uses_new_ref() {
        let store = TempStore::new();
        create_fixture_agent_g2(&store.root);
        let model = ScriptedModelFixture::new(vec!["first answer", "second answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the first Turn starts");
        hooks.wait_before_agent_run_attempt().await;
        let running = loaded.executor.snapshot().await.unwrap();
        assert_eq!(running.execution_state(), SessionExecutionState::Running);
        assert_eq!(running.definition().agent().revision().get(), 1);

        // The Workspace root disappears before the upgrade.  An Agent upgrade never invokes the
        // Workspace resolver, so it still succeeds without touching the Snapshot.
        fs::remove_dir_all(&store.old_workspace).unwrap();
        let upgraded = loaded
            .executor
            .upgrade_session_agent_with_cancellation(
                running.definition_revision(),
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await
            .expect("the Agent upgrade succeeds during Running without the resolver");
        assert!(upgraded.changed());
        assert_eq!(upgraded.definition_revision().get(), 2);

        // The installed snapshot keeps the exact WorkspaceSnapshot and active Turn while
        // carrying the new Agent ref: the already-captured Turn context is untouched.
        let during = loaded.executor.snapshot().await.unwrap();
        assert_eq!(during.execution_state(), SessionExecutionState::Running);
        assert_eq!(during.current_turn(), Some(turn_id));
        assert_eq!(during.definition_revision().get(), 2);
        assert_eq!(during.definition().agent().revision().get(), 2);
        assert!(Arc::ptr_eq(during.workspace(), running.workspace()));

        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);

        // A future admission reads the new installed definition and resolves the new Agent ref
        // from durable state, so it captures against ar_2.  The test PromptService only carries
        // the prompts patched into the ar_1 fixture definitions, so ar_2's prompt selection is
        // unavailable and the admission fails with Prompt instead of starting a second Turn.  If
        // the stale ar_1 capture were wrongly reused, the "second answer" model request would
        // run instead, so the Prompt failure itself proves the admission used the new Agent ref.
        assert_eq!(
            loaded
                .executor
                .submit(CommandId::generate().unwrap(), text_intent("second"))
                .await,
            Err(SessionSubmitError::Prompt)
        );
        let admitted = loaded.executor.snapshot().await.unwrap();
        assert_eq!(admitted.execution_state(), SessionExecutionState::Idle);
        assert_eq!(admitted.current_turn(), None);
        assert_eq!(admitted.definition().agent().revision().get(), 2);
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_terminal_race_keeps_follow_up_and_hands_off_against_new_ref() {
        let store = TempStore::new();
        create_fixture_agent_g2(&store.root);
        let model = ScriptedModelFixture::new(vec!["first answer", "second answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let hooks = loaded.executor.test_hooks();
        let follow_up_id = CommandId::generate().unwrap();

        // Hold the Agent upgrade between its durable commit and its snapshot install so the
        // first Turn terminal observes an active publication.
        hooks.arm_after_commit_before_install();
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the first Turn starts");
        hooks.wait_before_agent_run_attempt().await;
        let running = loaded.executor.snapshot().await.unwrap();
        assert_eq!(running.definition().agent().revision().get(), 1);
        let publication = {
            let executor = loaded.executor.clone();
            tokio::spawn(async move {
                executor
                    .upgrade_session_agent_with_cancellation(
                        running.definition_revision(),
                        None,
                        "2026-08-03T10:02:00.000Z".parse().unwrap(),
                        CommandId::generate().unwrap(),
                        CancellationToken::new(),
                    )
                    .await
            })
        };
        hooks.wait_after_commit_before_install().await;
        loaded
            .executor
            .follow_up(follow_up_id, text_intent("queued follow up"))
            .await
            .expect("the FollowUp is queued while the Turn runs");
        let queued = loaded.executor.snapshot().await.unwrap();
        assert_eq!(queued.follow_up_command_ids(), [follow_up_id]);

        // The Turn terminates while the publication is still active; the FollowUp stays queued.
        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        let terminal = loaded.executor.snapshot().await.unwrap();
        assert_eq!(terminal.follow_up_command_ids(), [follow_up_id]);
        assert_eq!(terminal.definition().agent().revision().get(), 1);

        // Once the upgrade settles, the queued FollowUp is handed off against the new Agent ref.
        // ar_2's prompts are unavailable in the test PromptService, so its admission capture
        // fails with Prompt and the FollowUp drains without a model request; reusing the stale
        // ar_1 capture would have produced the "second answer" model request instead.
        hooks.release_after_commit_before_install();
        assert!(publication.await.unwrap().unwrap().changed());
        assert_eq!(
            loaded
                .executor
                .snapshot()
                .await
                .unwrap()
                .definition()
                .agent()
                .revision()
                .get(),
            2
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = loaded.executor.snapshot().await.unwrap();
                if snapshot.follow_up_command_ids().is_empty()
                    && snapshot.execution_state() == SessionExecutionState::Idle
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the FollowUp leaves the queue after the upgrade settles");
        assert_eq!(
            loaded
                .executor
                .snapshot()
                .await
                .unwrap()
                .definition()
                .agent()
                .revision()
                .get(),
            2
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_upgrade_postcommit_install_failure_is_fatal() {
        let store = TempStore::new();
        create_fixture_agent_g2(&store.root);
        let loaded = loaded_fixture(&store).await;
        let hooks = loaded.executor.test_hooks();
        let before = loaded.executor.snapshot().await.unwrap();
        hooks.fail_next_snapshot_install_after_commit();
        let result = loaded
            .executor
            .upgrade_session_agent_with_cancellation(
                before.definition_revision(),
                None,
                "2026-08-03T10:02:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            result,
            Err(SessionDefinitionPublicationError::InternalDispatchUnavailable)
        );
        // The durable commit happened before the fatal install; the executor actor and shared
        // owners are poisoned and closing.
        let durable = loaded
            .state
            .session_current_definition(SESSION_ID.parse().unwrap())
            .unwrap();
        assert_eq!(durable.revision().get(), 2);
        assert_eq!(durable.agent().revision().get(), 2);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            loaded.executor.wait_until_closing_for_test(),
        )
        .await
        .expect("the poisoned executor enters closing");
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_workspace_while_idle_installs_new_snapshot_and_publishes_exact_event() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let mut subscription = loaded.executor.subscribe().await.unwrap();
        let before = loaded.executor.snapshot().await.unwrap();
        assert_eq!(before.definition_revision().get(), 1);
        assert_eq!(before.workspace_revision().get(), 1);
        assert_eq!(before.execution_state(), SessionExecutionState::Idle);

        let command_id = CommandId::generate().unwrap();
        let owner_timestamp = "2026-08-03T10:02:00.000Z".parse().unwrap();
        let reloaded = loaded
            .executor
            .reload_workspace_with_cancellation(
                owner_timestamp,
                command_id,
                CancellationToken::new(),
            )
            .await
            .expect("the loaded Idle Workspace reload publishes");
        assert!(reloaded.changed());
        assert_eq!(reloaded.definition_revision().get(), 1);
        assert_eq!(reloaded.workspace_revision().get(), 1);

        // The reload is always a real reload: a fresh WorkspaceSnapshot Arc is installed while
        // the exact installed definition Arc and Workspace revision are preserved.
        let after = loaded.executor.snapshot().await.unwrap();
        assert!(!Arc::ptr_eq(after.workspace(), before.workspace()));
        assert_eq!(after.workspace_revision().get(), 1);
        assert_eq!(
            after.workspace().session_id(),
            before.workspace().session_id()
        );
        assert!(Arc::ptr_eq(after.definition(), before.definition()));
        assert_eq!(after.definition_revision().get(), 1);
        assert_eq!(after.execution_state(), SessionExecutionState::Idle);
        assert_eq!(after.metadata().revision(), before.metadata().revision());
        // The durable definition is untouched: the Store generation is unchanged.
        assert_eq!(
            loaded
                .state
                .session_head(SESSION_ID.parse().unwrap())
                .unwrap()
                .storage_generation()
                .get(),
            1
        );

        // The exact WorkspaceReloaded executor event carries the owner command id and timestamp
        // with the post-install snapshot.
        let event = subscription
            .recv()
            .await
            .expect("the reload publishes one executor event");
        match event.as_ref() {
            SessionExecutorEvent::WorkspaceReloaded {
                timestamp,
                command_id: event_command_id,
                snapshot,
            } => {
                assert_eq!(*timestamp, owner_timestamp);
                assert_eq!(*event_command_id, command_id);
                assert_eq!(snapshot.workspace_revision().get(), 1);
                assert_eq!(snapshot.definition_revision().get(), 1);
            }
            _ => panic!("the reload publishes exactly one WorkspaceReloaded event"),
        }
        // No Recorder call and no conversation write.
        assert_eq!(
            fs::read(store.session_path().join("conversation.jsonl")).unwrap(),
            conversation_header_fixture()
        );
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_workspace_ordinary_failure_preserves_old_snapshot_and_no_event_then_recovers() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let mut subscription = loaded.executor.subscribe().await.unwrap();
        let before = loaded.executor.snapshot().await.unwrap();
        let before_definition = Arc::clone(before.definition());
        let before_workspace = Arc::clone(before.workspace());

        // A missing root is an ordinary resolver failure: the exact old snapshot Arc is kept and
        // no event is published.
        fs::remove_dir_all(&store.old_workspace).unwrap();
        assert_eq!(
            loaded
                .executor
                .reload_workspace_with_cancellation(
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::WorkspaceUnavailable)
        );
        let after_failure = loaded.executor.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&after_failure, &before));
        assert!(Arc::ptr_eq(after_failure.workspace(), &before_workspace));
        assert!(Arc::ptr_eq(after_failure.definition(), &before_definition));
        assert_eq!(after_failure.workspace_revision().get(), 1);
        assert_eq!(loaded.definition.revision().get(), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), subscription.recv())
                .await
                .is_err(),
            "a failed reload publishes no executor event"
        );

        // Restoring the root makes the next reload succeed with a fresh snapshot Arc.
        create_dir(&store.old_workspace);
        create_dir(&store.old_workspace.join("src"));
        let recovered = loaded
            .executor
            .reload_workspace_with_cancellation(
                "2026-08-03T10:03:00.000Z".parse().unwrap(),
                CommandId::generate().unwrap(),
                CancellationToken::new(),
            )
            .await
            .expect("the restored Workspace reloads");
        assert!(recovered.changed());
        assert_eq!(recovered.workspace_revision().get(), 1);
        let after_recovery = loaded.executor.snapshot().await.unwrap();
        assert!(!Arc::ptr_eq(after_recovery.workspace(), &before_workspace));
        assert_eq!(after_recovery.workspace_revision().get(), 1);
        assert!(Arc::ptr_eq(after_recovery.definition(), &before_definition));
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_workspace_during_running_is_busy_without_resolver_calls() {
        let store = TempStore::new();
        let model = ScriptedModelFixture::new(vec!["first answer"]);
        let loaded = scripted_text_fixture(&store, &model).await;
        let resolver_hooks = loaded.resolver.test_hooks();
        let hooks = loaded.executor.test_hooks();
        resolver_hooks.arm_after_candidate_before_final_recheck();

        // A reload during Running is Busy before any resolver call.
        hooks.arm_before_agent_run_attempt();
        let turn_id = loaded
            .executor
            .submit(CommandId::generate().unwrap(), text_intent("hello"))
            .await
            .expect("the first Turn starts");
        hooks.wait_before_agent_run_attempt().await;
        assert_eq!(
            loaded
                .executor
                .reload_workspace_with_cancellation(
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::SessionBusy)
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                resolver_hooks.wait_after_candidate_before_final_recheck(),
            )
            .await
            .is_err(),
            "the Busy reload never invokes the resolver"
        );
        let busy_snapshot = loaded.executor.snapshot().await.unwrap();
        assert_eq!(
            busy_snapshot.execution_state(),
            SessionExecutionState::Running
        );

        hooks.release_before_agent_run_attempt();
        assert_eq!(
            wait_for_terminal(&loaded.executor, turn_id).await,
            SessionTurnTerminal::Completed
        );
        assert_eq!(model.request_count(), 1);
        close_loaded(loaded).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reload_workspace_uses_the_single_publication_slot_both_directions() {
        let store = TempStore::new();
        let loaded = loaded_fixture(&store).await;
        let resolver_hooks = loaded.resolver.test_hooks();
        let hooks = loaded.executor.test_hooks();

        // A reload while another publication occupies the single active slot is Busy.
        hooks.arm_after_candidate_snapshot_finish_before_durable();
        let idle = loaded.executor.snapshot().await.unwrap();
        let executor = loaded.executor.clone();
        let new_workspace = store.new_workspace.clone();
        let publication = tokio::spawn(async move {
            executor
                .update_workspace_definition(
                    idle.definition_revision(),
                    changed_workspace(&new_workspace),
                    "2026-08-03T10:02:00.000Z".parse().unwrap(),
                )
                .await
        });
        hooks
            .wait_after_candidate_snapshot_finish_before_durable()
            .await;
        assert_eq!(
            loaded
                .executor
                .reload_workspace_with_cancellation(
                    "2026-08-03T10:03:00.000Z".parse().unwrap(),
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await,
            Err(SessionDefinitionPublicationError::SessionBusy)
        );
        hooks.release_after_candidate_snapshot_finish_before_durable();
        assert!(publication.await.unwrap().unwrap().changed());

        // The reverse exclusion also holds: a reload in flight makes an ordinary update Busy.
        resolver_hooks.arm_after_candidate_before_final_recheck();
        let reloaded_executor = loaded.executor.clone();
        let reload = tokio::spawn(async move {
            reloaded_executor
                .reload_workspace_with_cancellation(
                    "2026-08-03T10:04:00.000Z".parse().unwrap(),
                    CommandId::generate().unwrap(),
                    CancellationToken::new(),
                )
                .await
        });
        // Hold the reload worker at the resolver barrier so the publication slot is occupied.
        resolver_hooks
            .wait_after_candidate_before_final_recheck()
            .await;
        let running = loaded.executor.snapshot().await.unwrap();
        assert_eq!(
            loaded
                .executor
                .update_workspace_definition(
                    running.definition_revision(),
                    changed_workspace(&store.new_workspace),
                    "2026-08-03T10:05:00.000Z".parse().unwrap(),
                )
                .await,
            Err(SessionWorkspaceDefinitionError::SessionBusy)
        );
        resolver_hooks.release_after_candidate_before_final_recheck();
        let outcome = reload.await.unwrap().expect("the held reload completes");
        assert!(outcome.changed());
        close_loaded(loaded).await;
    }
}
