use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

#[cfg(test)]
use std::collections::VecDeque;

use crate::compaction::{
    CompactionReplacement, CompactionUnitKind, LiveCompactionSourceView, PreparedLiveCompactionUnit,
};
use crate::prompt::{ModelAssistantContent, ModelMessage};
use crate::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, InteractionRequest, InteractionRequestView,
    InteractionResolution, InteractionResolutionInput, InteractionResolutionViewRef,
    InteractionValueError, ItemContentFamily, ItemRelation, ResolvedInteraction, UserMessageSource,
};
use crate::wire::{
    EntryId, InteractionResolutionKey, ItemId, RequestId, SessionId, Timestamp, TurnId,
};

use super::{
    StoredAssistantContent, StoredAssistantMessage, StoredEntryBody, StoredInteractionRequest,
    StoredInteractionRequestBody, StoredInteractionResolution, StoredInteractionResolutionBody,
    StoredSessionEntry, StoredToolMessage, StoredToolOutcome, StoredUserMessage,
};

const MAX_ENTRY_ID_ALLOCATION_ATTEMPTS: usize = 32;

/// Immutable, sanitized live conversation projection consumed by ordinary Prompt assembly.
///
/// This type deliberately lives beside `LiveSessionState`: its constructor is private to this
/// module, so neither Conversation Storage nor another crate module can forge a view at an
/// arbitrary revision.
#[derive(Clone)]
pub(crate) struct LiveConversationView {
    revision: ConversationRevision,
    messages: Arc<[ModelMessage]>,
}

impl LiveConversationView {
    fn from_live_state(revision: ConversationRevision, messages: Arc<[ModelMessage]>) -> Self {
        Self { revision, messages }
    }

    pub(crate) const fn revision(&self) -> ConversationRevision {
        self.revision
    }

    pub(crate) fn messages(&self) -> &[ModelMessage] {
        &self.messages
    }
}

#[derive(Clone, Copy, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConversationRevision(u64);

impl ConversationRevision {
    /// Reconstructs the process-local revision counted by the bounded cold replay projection. The
    /// count covers only facts that survive model-visible replay semantics; it is not a physical
    /// line counter. The caller owns the checked operation count; this constructor is not a wire
    /// or public ID ingress.
    pub(crate) const fn from_replay_operations(value: u64) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) const fn operation_count_for_test(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Result<Self, LiveConversationError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(LiveConversationError::new(
                LiveConversationErrorReason::RevisionOverflow,
            ))
    }
}

impl fmt::Debug for ConversationRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConversationRevision(<process-local>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LiveConversationErrorReason {
    RevisionOverflow,
    EntryIdAllocation,
    InvalidRelation,
    InvalidTurn,
    InvalidPromptProjection,
    InvalidCompactionSource,
    StaleCompactionSource,
    InvalidCompactionCut,
    CompactionMarkerMismatch,
    PendingToolExchange,
    InteractionConflict,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct LiveConversationError {
    reason: LiveConversationErrorReason,
}

impl LiveConversationError {
    const fn new(reason: LiveConversationErrorReason) -> Self {
        Self { reason }
    }

    pub(crate) const fn reason(&self) -> LiveConversationErrorReason {
        self.reason
    }
}

impl fmt::Debug for LiveConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveConversationError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for LiveConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("live conversation operation failed")
    }
}

impl Error for LiveConversationError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum EntryIdAllocationError {
    EntropyUnavailable,
    CollisionAttemptsExhausted,
}

impl fmt::Display for EntryIdAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("entry identifier allocation failed")
    }
}

impl Error for EntryIdAllocationError {}

pub(crate) struct EntryIdGenerator {
    reserved: BTreeSet<EntryId>,
}

impl EntryIdGenerator {
    pub(crate) fn new(reserved: impl IntoIterator<Item = EntryId>) -> Self {
        Self {
            reserved: reserved.into_iter().collect(),
        }
    }

    pub(crate) fn allocate(&mut self) -> Result<EntryId, EntryIdAllocationError> {
        self.allocate_candidates(EntryId::generate)
    }

    fn allocate_candidates<F, E>(
        &mut self,
        mut next_candidate: F,
    ) -> Result<EntryId, EntryIdAllocationError>
    where
        F: FnMut() -> Result<EntryId, E>,
    {
        for _ in 0..MAX_ENTRY_ID_ALLOCATION_ATTEMPTS {
            let candidate =
                next_candidate().map_err(|_| EntryIdAllocationError::EntropyUnavailable)?;
            if self.reserved.insert(candidate) {
                return Ok(candidate);
            }
        }
        Err(EntryIdAllocationError::CollisionAttemptsExhausted)
    }
}

impl fmt::Debug for EntryIdGenerator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EntryIdGenerator")
            .field("reserved_count", &self.reserved.len())
            .finish()
    }
}

pub(crate) struct AppliedConversationFact {
    entry: Arc<StoredSessionEntry>,
    revision: ConversationRevision,
}

impl AppliedConversationFact {
    pub(crate) const fn entry(&self) -> &Arc<StoredSessionEntry> {
        &self.entry
    }

    pub(crate) const fn revision(&self) -> ConversationRevision {
        self.revision
    }
}

pub(crate) enum InteractionResolutionApplyOutcome {
    Applied(AppliedConversationFact),
    Idempotent { revision: ConversationRevision },
}

pub(crate) enum HostInteractionResolutionApplyOutcome {
    Applied {
        fact: AppliedConversationFact,
        resolution: ResolvedInteraction,
    },
    Idempotent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostInteractionResolutionError {
    NotFound,
    ExpectedTurnMismatch,
    FamilyMismatch,
    InvalidResolution,
    AlreadyResolved,
    CommandConflict,
    Internal,
}

pub(crate) struct InteractionRequestCandidate {
    request_id: RequestId,
    item_id: ItemId,
    request: InteractionRequest,
}

impl InteractionRequestCandidate {
    pub(crate) fn new(request_id: RequestId, item_id: ItemId, request: InteractionRequest) -> Self {
        Self {
            request_id,
            item_id,
            request,
        }
    }
}

impl fmt::Debug for InteractionRequestCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractionRequestCandidate")
            .field("request_id", &self.request_id)
            .field("item_id", &self.item_id)
            .field("request", &"redacted")
            .finish()
    }
}

pub(crate) struct InteractionResolutionCandidate {
    request_id: RequestId,
    resolution_key: Option<InteractionResolutionKey>,
    resolution: ResolvedInteraction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InteractionCandidateErrorReason {
    InvalidResolutionOrigin,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct InteractionCandidateError {
    reason: InteractionCandidateErrorReason,
}

impl InteractionCandidateError {
    const fn invalid_resolution_origin() -> Self {
        Self {
            reason: InteractionCandidateErrorReason::InvalidResolutionOrigin,
        }
    }
}

impl fmt::Debug for InteractionCandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractionCandidateError")
            .field("reason", &self.reason)
            .finish()
    }
}

impl fmt::Display for InteractionCandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid interaction resolution candidate")
    }
}

impl Error for InteractionCandidateError {}

impl InteractionResolutionCandidate {
    pub(crate) fn host(
        request_id: RequestId,
        resolution_key: InteractionResolutionKey,
        resolution: ResolvedInteraction,
    ) -> Result<Self, InteractionCandidateError> {
        if !matches!(
            resolution.live(),
            InteractionResolution::ToolApproval(_)
                | InteractionResolution::UserAnswer(_)
                | InteractionResolution::Cancelled(InteractionCancelReason::HostCancelled)
        ) {
            return Err(InteractionCandidateError::invalid_resolution_origin());
        }
        Ok(Self {
            request_id,
            resolution_key: Some(resolution_key),
            resolution,
        })
    }

    pub(crate) fn owner_cancellation(
        request_id: RequestId,
        reason: InteractionCancelReason,
    ) -> Result<Self, InteractionCandidateError> {
        let resolution = ResolvedInteraction::cancelled_by_owner(reason)
            .ok_or(InteractionCandidateError::invalid_resolution_origin())?;
        Ok(Self {
            request_id,
            resolution_key: None,
            resolution,
        })
    }
}

impl fmt::Debug for InteractionResolutionCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractionResolutionCandidate")
            .field("request_id", &self.request_id)
            .field("has_resolution_key", &self.resolution_key.is_some())
            .field("resolution", &"redacted")
            .finish()
    }
}

struct Interaction {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequest,
    state: InteractionState,
}

enum InteractionState {
    Pending,
    Resolved {
        resolution: ResolvedInteraction,
        resolution_key: Option<InteractionResolutionKey>,
    },
}

#[derive(Clone)]
pub(crate) struct PendingInteractionFact {
    request_id: RequestId,
    turn_id: TurnId,
    item_id: ItemId,
    request: InteractionRequestView,
}

impl PendingInteractionFact {
    pub(crate) const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub(crate) const fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub(crate) const fn item_id(&self) -> &ItemId {
        &self.item_id
    }

    pub(crate) const fn request(&self) -> &InteractionRequestView {
        &self.request
    }
}

pub(crate) struct CapturedConversationViews {
    conversation: LiveConversationView,
    compaction_source: Arc<LiveCompactionSourceView>,
    selected_head: Option<EntryId>,
    relations: Arc<[ItemRelation]>,
    pending_interactions: Arc<[PendingInteractionFact]>,
}

impl CapturedConversationViews {
    pub(crate) const fn conversation(&self) -> &LiveConversationView {
        &self.conversation
    }

    pub(crate) const fn compaction_source(&self) -> &Arc<LiveCompactionSourceView> {
        &self.compaction_source
    }

    pub(crate) const fn selected_head(&self) -> Option<&EntryId> {
        self.selected_head.as_ref()
    }

    pub(crate) fn relations(&self) -> &[ItemRelation] {
        &self.relations
    }

    pub(crate) fn pending_interactions(&self) -> &[PendingInteractionFact] {
        &self.pending_interactions
    }
}

#[derive(Clone)]
struct ExpectedToolCall {
    item_id: ItemId,
    tool_call_id: crate::tools::ToolCallId,
    terminal: Option<StoredToolMessage>,
}

#[derive(Clone)]
struct PendingToolExchange {
    assistant_entry_id: EntryId,
    assistant_message: ModelMessage,
    expected: Vec<ExpectedToolCall>,
}

enum PreparedToolStateDelta {
    Pending(PendingToolExchange),
    Complete(crate::compaction::LiveCompactionUnit),
}

#[cfg(test)]
#[derive(Clone, Eq, PartialEq)]
struct ScriptedEntryIdCandidates {
    candidates: VecDeque<Result<EntryId, ()>>,
    allocation_calls: usize,
}

/// The loaded-session, in-memory transaction owner for recordable conversation facts.
pub(crate) struct LiveSessionState {
    session_id: SessionId,
    selected_path: Vec<Arc<StoredSessionEntry>>,
    entry_ids: EntryIdGenerator,
    revision: ConversationRevision,
    relations: Vec<ItemRelation>,
    interactions: Vec<Interaction>,
    stable_units: Vec<crate::compaction::LiveCompactionUnit>,
    current_turn: Option<TurnId>,
    tool_exchange: Option<PendingToolExchange>,
    #[cfg(test)]
    scripted_entry_ids: Option<ScriptedEntryIdCandidates>,
    #[cfg(test)]
    fail_prepared_unit_preflight: bool,
}

impl LiveSessionState {
    pub(crate) fn selected_entries(&self) -> &[Arc<StoredSessionEntry>] {
        &self.selected_path
    }

    pub(crate) fn capture_fork_conversation(
        &self,
        anchor: crate::agent_session_lifecycle::ForkAnchor,
    ) -> Result<super::CapturedForkConversation, super::ForkAnchorResolutionError> {
        super::CapturedForkConversation::from_selected_path(
            self.session_id,
            crate::agent_session_lifecycle::ForkSourceKind::LiveSnapshot,
            anchor,
            &self.selected_path,
            &self.relations,
        )
    }

    /// Creates a fresh loaded session. Every supplied historical/reserved ID is seeded into the
    /// collision guard before the first live allocation.
    pub(crate) fn new(
        session_id: SessionId,
        reserved_entry_ids: impl IntoIterator<Item = EntryId>,
    ) -> Self {
        Self {
            session_id,
            selected_path: Vec::new(),
            entry_ids: EntryIdGenerator::new(reserved_entry_ids),
            revision: ConversationRevision::default(),
            relations: Vec::new(),
            interactions: Vec::new(),
            stable_units: Vec::new(),
            current_turn: None,
            tool_exchange: None,
            #[cfg(test)]
            scripted_entry_ids: None,
            #[cfg(test)]
            fail_prepared_unit_preflight: false,
        }
    }

    /// Seeds a loaded Session from Conversation Storage's already-sanitized cold projection.
    /// This constructor is intentionally separate from every strict live `apply_*` method:
    /// tolerant replay may have isolated facts that a live mutation would reject, and replay
    /// must never manufacture a new EntryId or re-run live validation as a repair operation.
    pub(crate) fn from_replayed_view(
        session_id: SessionId,
        view: &super::ReplayedConversationView,
    ) -> Result<Self, LiveConversationError> {
        if view.header().session_id() != session_id {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }

        let selected_entries = view.selected_entries();
        let mut selected_ids = BTreeSet::new();
        let mut previous = None;
        for (index, entry) in selected_entries.iter().enumerate() {
            if entry.session_id() != session_id
                || !selected_ids.insert(entry.entry_id())
                || (index == 0 && entry.parent_id().is_some())
                || (index != 0 && entry.parent_id() != previous)
            {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InvalidRelation,
                ));
            }
            previous = Some(entry.entry_id());
        }
        if previous != view.selected_head() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }

        let reserved = view.reserved_ids().iter().copied().collect::<BTreeSet<_>>();
        if selected_ids
            .iter()
            .any(|entry_id| !reserved.contains(entry_id))
        {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }

        let relations = view.relations().to_vec();
        let mut relation_items = BTreeSet::new();
        if relations
            .iter()
            .any(|relation| !relation_items.insert(relation.item_id()))
        {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }

        let stable_units = view.stable_units().to_vec();
        if stable_units
            .iter()
            .any(|unit| !selected_ids.contains(unit.first_entry_id()))
        {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidCompactionSource,
            ));
        }
        LiveCompactionSourceView::for_replay(
            session_id,
            view.revision(),
            stable_units.clone().into(),
        )
        .map_err(|_| {
            LiveConversationError::new(LiveConversationErrorReason::InvalidCompactionSource)
        })?;

        Ok(Self {
            session_id,
            selected_path: selected_entries.to_vec(),
            entry_ids: EntryIdGenerator::new(reserved),
            revision: view.revision(),
            relations,
            // Recorded Interaction facts remain in the selected history, but replay never
            // reconstructs a process-local waiter or the private approval mapping needed for a
            // live pending Interaction.
            interactions: Vec::new(),
            stable_units,
            current_turn: None,
            tool_exchange: None,
            #[cfg(test)]
            scripted_entry_ids: None,
            #[cfg(test)]
            fail_prepared_unit_preflight: false,
        })
    }

    pub(crate) fn apply_user_message(
        &mut self,
        body: StoredUserMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        Self::validate_stored_body(&StoredEntryBody::UserMessage(body.clone()))?;
        match body.source() {
            UserMessageSource::Input if self.current_turn.is_none() => {}
            UserMessageSource::Steer if self.current_turn == Some(turn_id) => {}
            UserMessageSource::Input | UserMessageSource::Steer => {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InvalidTurn,
                ));
            }
        }
        if body.source() == UserMessageSource::Steer && self.tool_exchange.is_some() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::PendingToolExchange,
            ));
        }

        let relation = ItemRelation::user_message(body.item_id(), turn_id);
        self.validate_new_relations(std::slice::from_ref(&relation))?;
        let message = ModelMessage::canonical_user(body.content().clone());
        #[cfg(test)]
        self.test_prepared_unit_preflight()?;
        let prepared = Self::prepare_unit(CompactionUnitKind::UserMessage, vec![message])?;
        let next_revision = self.revision.checked_next()?;

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            turn_id,
            timestamp,
            StoredEntryBody::UserMessage(body),
        );
        let stable_unit = prepared.bind_origin(entry_id);

        if self.current_turn.is_none() {
            self.current_turn = Some(turn_id);
        }
        self.relations.push(relation);
        self.stable_units.push(stable_unit);
        self.selected_path.push(entry.clone());
        self.revision = next_revision;
        Ok(AppliedConversationFact {
            entry,
            revision: self.revision,
        })
    }

    pub(crate) fn apply_assistant_message(
        &mut self,
        body: StoredAssistantMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        Self::validate_stored_body(&StoredEntryBody::AssistantMessage(body.clone()))?;
        self.require_current_turn(turn_id)?;
        if self.tool_exchange.is_some() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::PendingToolExchange,
            ));
        }

        let (message, relations, expected) = Self::project_assistant(&body, turn_id)?;
        self.validate_new_relations(&relations)?;
        let prepared = if expected.is_empty() {
            #[cfg(test)]
            self.test_prepared_unit_preflight()?;
            Some(Self::prepare_unit(
                CompactionUnitKind::AssistantMessage,
                vec![message.clone()],
            )?)
        } else {
            None
        };
        let next_revision = self.revision.checked_next()?;

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            turn_id,
            timestamp,
            StoredEntryBody::AssistantMessage(body),
        );
        let stable_unit = prepared.map(|unit| unit.bind_origin(entry_id));
        let exchange = if expected.is_empty() {
            None
        } else {
            Some(PendingToolExchange {
                assistant_entry_id: entry_id,
                assistant_message: message,
                expected,
            })
        };

        self.relations.extend(relations);
        if let Some(unit) = stable_unit {
            self.stable_units.push(unit);
        }
        self.tool_exchange = exchange;
        self.selected_path.push(entry.clone());
        self.revision = next_revision;
        Ok(AppliedConversationFact {
            entry,
            revision: self.revision,
        })
    }

    pub(crate) fn complete_with_assistant_message(
        &mut self,
        body: StoredAssistantMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        if body.disposition() != AssistantDisposition::Final
            || body
                .content()
                .iter()
                .any(|content| matches!(content, StoredAssistantContent::ToolCall { .. }))
        {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidTurn,
            ));
        }
        let fact = self.apply_assistant_message(body, turn_id, timestamp)?;
        debug_assert!(self.tool_exchange.is_none());
        self.current_turn = None;
        Ok(fact)
    }

    pub(crate) fn fail_current_turn(
        &mut self,
        turn_id: TurnId,
    ) -> Result<(), LiveConversationError> {
        self.require_current_turn(turn_id)?;
        if self.tool_exchange.is_some() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::PendingToolExchange,
            ));
        }
        self.current_turn = None;
        Ok(())
    }

    pub(crate) fn abandon_current_tool_exchange(
        &mut self,
        turn_id: TurnId,
    ) -> Result<(), LiveConversationError> {
        self.require_current_turn(turn_id)?;
        if self.tool_exchange.is_none() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }
        self.tool_exchange = None;
        self.current_turn = None;
        Ok(())
    }

    pub(crate) const fn current_turn(&self) -> Option<TurnId> {
        self.current_turn
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn apply_tool_message(
        &mut self,
        body: StoredToolMessage,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        Self::validate_stored_body(&StoredEntryBody::ToolMessage(body.clone()))?;
        self.require_current_turn(turn_id)?;

        let Some(exchange) = self.tool_exchange.as_ref() else {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        };
        let Some(expected_index) = exchange.expected.iter().position(|expected| {
            expected.item_id == body.item_id() && expected.tool_call_id == *body.tool_call_id()
        }) else {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        };
        let Some(expected) = exchange.expected.get(expected_index) else {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        };
        if expected.terminal.is_some() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }
        if !self.relations.iter().any(|relation| {
            relation.item_id() == body.item_id()
                && relation.turn_id() == turn_id
                && relation.family() == ItemContentFamily::ToolInvocation
                && relation.tool_call_id() == Some(body.tool_call_id())
        }) {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }
        if self.has_pending_interaction_for_item(body.item_id()) {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InteractionConflict,
            ));
        }

        let completes_exchange = exchange
            .expected
            .iter()
            .enumerate()
            .all(|(index, expected)| {
                if index == expected_index {
                    Self::is_completed_tool(&body)
                } else {
                    expected
                        .terminal
                        .as_ref()
                        .is_some_and(Self::is_completed_tool)
                }
            });
        let state_delta = if completes_exchange {
            #[cfg(test)]
            self.test_prepared_unit_preflight()?;
            let complete_exchange =
                Self::prepare_complete_exchange(exchange, expected_index, &body)?
                    .bind_origin(exchange.assistant_entry_id);
            PreparedToolStateDelta::Complete(complete_exchange)
        } else {
            let mut next_exchange = exchange.clone();
            let Some(expected) = next_exchange.expected.get_mut(expected_index) else {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InvalidRelation,
                ));
            };
            expected.terminal = Some(body.clone());
            PreparedToolStateDelta::Pending(next_exchange)
        };
        let next_revision = if completes_exchange {
            self.revision.checked_next()?
        } else {
            self.revision
        };

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            turn_id,
            timestamp,
            StoredEntryBody::ToolMessage(body),
        );

        match state_delta {
            PreparedToolStateDelta::Pending(exchange) => self.tool_exchange = Some(exchange),
            PreparedToolStateDelta::Complete(unit) => {
                self.tool_exchange = None;
                self.stable_units.push(unit);
            }
        }
        self.selected_path.push(entry.clone());
        self.revision = next_revision;
        Ok(AppliedConversationFact {
            entry,
            revision: self.revision,
        })
    }

    pub(crate) fn apply_interaction_request(
        &mut self,
        candidate: InteractionRequestCandidate,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        self.require_current_turn(turn_id)?;
        if !self.is_started_tool_invocation(candidate.item_id, turn_id) {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidRelation,
            ));
        }
        if self
            .interactions
            .iter()
            .any(|interaction| interaction.request_id == candidate.request_id)
            || self.interactions.iter().any(|interaction| {
                interaction.item_id == candidate.item_id
                    && matches!(interaction.state, InteractionState::Pending)
            })
        {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InteractionConflict,
            ));
        }

        let stored = StoredInteractionRequest::reconstruct(
            candidate.request_id,
            candidate.item_id,
            Self::stored_request_body(&candidate.request),
        );
        Self::validate_stored_body(&StoredEntryBody::InteractionRequested(stored.clone()))?;

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            turn_id,
            timestamp,
            StoredEntryBody::InteractionRequested(stored),
        );
        let interaction = Interaction {
            request_id: candidate.request_id,
            turn_id,
            item_id: candidate.item_id,
            request: candidate.request,
            state: InteractionState::Pending,
        };

        self.interactions.push(interaction);
        self.selected_path.push(entry.clone());
        Ok(AppliedConversationFact {
            entry,
            revision: self.revision,
        })
    }

    pub(crate) fn apply_interaction_resolution(
        &mut self,
        candidate: InteractionResolutionCandidate,
        timestamp: Timestamp,
    ) -> Result<InteractionResolutionApplyOutcome, LiveConversationError> {
        let Some(interaction_index) = self
            .interactions
            .iter()
            .position(|interaction| interaction.request_id == candidate.request_id)
        else {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InteractionConflict,
            ));
        };
        let interaction = &self.interactions[interaction_index];
        let interaction_turn_id = interaction.turn_id;
        let resolution_key = candidate.resolution_key.clone();
        interaction
            .request
            .validate_exact_resolution(&candidate.resolution)
            .map_err(|_| {
                LiveConversationError::new(LiveConversationErrorReason::InteractionConflict)
            })?;
        let stored = Self::stored_resolution(
            interaction.request_id,
            interaction.item_id,
            &interaction.request,
            &candidate.resolution,
            resolution_key.clone(),
        )?;

        if let Some(key) = resolution_key.as_ref() {
            if self.interactions.iter().any(|other| {
                other.request_id != candidate.request_id
                    && matches!(
                        &other.state,
                        InteractionState::Resolved {
                            resolution_key: Some(other_key),
                            ..
                        } if other_key == key
                    )
            }) {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InteractionConflict,
                ));
            }
        }

        if let InteractionState::Resolved {
            resolution,
            resolution_key: existing_key,
            ..
        } = &interaction.state
        {
            let existing = Self::stored_resolution(
                interaction.request_id,
                interaction.item_id,
                &interaction.request,
                resolution,
                existing_key.clone(),
            )?;
            if let (Some(existing_key), Some(candidate_key)) =
                (existing_key.as_ref(), resolution_key.as_ref())
            {
                if existing_key == candidate_key && existing == stored {
                    return Ok(InteractionResolutionApplyOutcome::Idempotent {
                        revision: self.revision,
                    });
                }
            }
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InteractionConflict,
            ));
        }

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            interaction_turn_id,
            timestamp,
            StoredEntryBody::InteractionResolved(stored),
        );
        // All fallible family/key validation completed above. The index remains valid because the
        // reducer is a single synchronous owner.
        self.interactions[interaction_index].state = InteractionState::Resolved {
            resolution: candidate.resolution,
            resolution_key,
        };
        self.selected_path.push(entry.clone());
        Ok(InteractionResolutionApplyOutcome::Applied(
            AppliedConversationFact {
                entry,
                revision: self.revision,
            },
        ))
    }

    pub(crate) fn apply_host_interaction_resolution(
        &mut self,
        request_id: RequestId,
        expected_turn_id: TurnId,
        item_id: ItemId,
        resolution_key: InteractionResolutionKey,
        input: InteractionResolutionInput,
        timestamp: Timestamp,
    ) -> Result<HostInteractionResolutionApplyOutcome, HostInteractionResolutionError> {
        let interaction = self
            .interactions
            .iter()
            .find(|interaction| interaction.request_id == request_id)
            .ok_or(HostInteractionResolutionError::NotFound)?;
        if interaction.turn_id != expected_turn_id {
            return Err(HostInteractionResolutionError::ExpectedTurnMismatch);
        }
        if interaction.item_id != item_id {
            return Err(HostInteractionResolutionError::NotFound);
        }
        let resolution = interaction
            .request
            .resolve_host(input)
            .map_err(|error| match error {
                InteractionValueError::FamilyMismatch => {
                    HostInteractionResolutionError::FamilyMismatch
                }
                InteractionValueError::InvalidResolution => {
                    HostInteractionResolutionError::InvalidResolution
                }
            })?;
        if let InteractionState::Resolved {
            resolution: existing,
            resolution_key: existing_key,
        } = &interaction.state
        {
            return match existing_key.as_ref() {
                Some(existing_key)
                    if existing_key == &resolution_key && existing == &resolution =>
                {
                    Ok(HostInteractionResolutionApplyOutcome::Idempotent)
                }
                Some(existing_key) if existing_key == &resolution_key => {
                    Err(HostInteractionResolutionError::CommandConflict)
                }
                Some(_) | None => Err(HostInteractionResolutionError::AlreadyResolved),
            };
        }
        let resolution_for_owner = resolution.clone_for_owner();
        let candidate =
            InteractionResolutionCandidate::host(request_id, resolution_key, resolution)
                .map_err(|_| HostInteractionResolutionError::Internal)?;
        match self.apply_interaction_resolution(candidate, timestamp) {
            Ok(InteractionResolutionApplyOutcome::Applied(fact)) => {
                Ok(HostInteractionResolutionApplyOutcome::Applied {
                    fact,
                    resolution: resolution_for_owner,
                })
            }
            Ok(InteractionResolutionApplyOutcome::Idempotent { .. }) => {
                Err(HostInteractionResolutionError::Internal)
            }
            Err(_) => Err(HostInteractionResolutionError::Internal),
        }
    }

    pub(crate) fn apply_compaction(
        &mut self,
        source: Arc<LiveCompactionSourceView>,
        cut: NonZeroUsize,
        replacement: CompactionReplacement,
        turn_id: TurnId,
        timestamp: Timestamp,
    ) -> Result<AppliedConversationFact, LiveConversationError> {
        self.require_current_turn(turn_id)?;
        if self.tool_exchange.is_some() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::PendingToolExchange,
            ));
        }

        let fresh_source = self.fresh_compaction_source()?;
        if !source.has_same_stable_identity(&fresh_source) {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::StaleCompactionSource,
            ));
        }
        let cut_index = cut.get();
        if cut_index > fresh_source.units().len() {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidCompactionCut,
            ));
        }
        let marker = fresh_source
            .units()
            .get(cut_index)
            .map(|unit| *unit.first_entry_id());
        let (stored, rolling_summary) = replacement.into_parts();
        #[cfg(test)]
        self.test_prepared_unit_preflight()?;
        let prepared =
            Self::prepare_unit(CompactionUnitKind::RollingSummary, vec![rolling_summary])?;
        if stored.first_kept_entry_id() != marker {
            return Err(LiveConversationError::new(
                LiveConversationErrorReason::CompactionMarkerMismatch,
            ));
        }
        let next_revision = self.revision.checked_next()?;
        let retained_units = fresh_source.units()[cut_index..].to_vec();

        let entry_id = self.allocate_entry_id()?;
        let entry = self.make_entry(
            entry_id,
            turn_id,
            timestamp,
            StoredEntryBody::Compaction(stored),
        );
        let summary_unit = prepared.bind_origin(entry_id);
        let mut replacement_units = Vec::with_capacity(retained_units.len() + 1);
        replacement_units.push(summary_unit);
        replacement_units.extend(retained_units);

        self.stable_units = replacement_units;
        self.selected_path.push(entry.clone());
        self.revision = next_revision;
        Ok(AppliedConversationFact {
            entry,
            revision: self.revision,
        })
    }

    pub(crate) fn capture_conversation_views(
        &self,
    ) -> Result<CapturedConversationViews, LiveConversationError> {
        let source = self.fresh_compaction_source()?;
        let messages = self.flatten_stable_messages();
        let pending_interactions = self.pending_interaction_facts().to_vec();

        Ok(CapturedConversationViews {
            conversation: LiveConversationView::from_live_state(self.revision, messages.into()),
            compaction_source: source,
            selected_head: self.selected_path.last().map(|entry| entry.entry_id()),
            relations: self.relations.clone().into(),
            pending_interactions: pending_interactions.into(),
        })
    }

    pub(crate) fn pending_interaction_facts(&self) -> Arc<[PendingInteractionFact]> {
        self.interactions
            .iter()
            .filter_map(|interaction| {
                if matches!(interaction.state, InteractionState::Pending) {
                    Some(PendingInteractionFact {
                        request_id: interaction.request_id,
                        turn_id: interaction.turn_id,
                        item_id: interaction.item_id,
                        request: interaction.request.view(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn require_current_turn(&self, turn_id: TurnId) -> Result<(), LiveConversationError> {
        if self.current_turn == Some(turn_id) {
            Ok(())
        } else {
            Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidTurn,
            ))
        }
    }

    fn validate_stored_body(body: &StoredEntryBody) -> Result<(), LiveConversationError> {
        body.validate_for_wire()
            .map_err(|_| LiveConversationError::new(LiveConversationErrorReason::InvalidRelation))
    }

    fn validate_new_relations(
        &self,
        candidates: &[ItemRelation],
    ) -> Result<(), LiveConversationError> {
        let mut item_ids = BTreeSet::new();
        for candidate in candidates {
            if self
                .relations
                .iter()
                .any(|existing| existing.item_id() == candidate.item_id())
                || !item_ids.insert(candidate.item_id())
            {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InvalidRelation,
                ));
            }
        }
        Ok(())
    }

    fn project_assistant(
        body: &StoredAssistantMessage,
        turn_id: TurnId,
    ) -> Result<(ModelMessage, Vec<ItemRelation>, Vec<ExpectedToolCall>), LiveConversationError>
    {
        let mut content = Vec::with_capacity(body.content().len());
        let mut relations = Vec::with_capacity(body.content().len());
        let mut expected = Vec::new();
        for item in body.content() {
            match item {
                StoredAssistantContent::Reasoning {
                    item_id,
                    content: value,
                } => {
                    content.push(ModelAssistantContent::reasoning(value.clone()));
                    relations.push(ItemRelation::reasoning(*item_id, turn_id));
                }
                StoredAssistantContent::Text { item_id, text } => {
                    content.push(ModelAssistantContent::text(text.clone()).map_err(|_| {
                        LiveConversationError::new(
                            LiveConversationErrorReason::InvalidPromptProjection,
                        )
                    })?);
                    relations.push(ItemRelation::agent_message(*item_id, turn_id));
                }
                StoredAssistantContent::ToolCall {
                    item_id,
                    tool_call_id,
                    name,
                    arguments,
                } => {
                    content.push(ModelAssistantContent::tool_call(
                        tool_call_id.clone(),
                        name.clone(),
                        arguments.clone(),
                    ));
                    relations.push(ItemRelation::tool_invocation(
                        *item_id,
                        turn_id,
                        tool_call_id.clone(),
                    ));
                    expected.push(ExpectedToolCall {
                        item_id: *item_id,
                        tool_call_id: tool_call_id.clone(),
                        terminal: None,
                    });
                }
            }
        }
        let message = ModelMessage::assistant(content.into()).map_err(|_| {
            LiveConversationError::new(LiveConversationErrorReason::InvalidPromptProjection)
        })?;
        Ok((message, relations, expected))
    }

    fn is_completed_tool(message: &StoredToolMessage) -> bool {
        matches!(message.outcome(), StoredToolOutcome::Completed { .. })
    }

    fn prepare_complete_exchange(
        exchange: &PendingToolExchange,
        current_index: usize,
        current: &StoredToolMessage,
    ) -> Result<PreparedLiveCompactionUnit, LiveConversationError> {
        let mut messages = Vec::with_capacity(exchange.expected.len() + 1);
        messages.push(exchange.assistant_message.clone());
        for (index, expected) in exchange.expected.iter().enumerate() {
            let terminal = if index == current_index {
                current
            } else {
                expected.terminal.as_ref().ok_or_else(|| {
                    LiveConversationError::new(LiveConversationErrorReason::InvalidRelation)
                })?
            };
            let StoredToolOutcome::Completed { content, .. } = terminal.outcome() else {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InvalidRelation,
                ));
            };
            messages.push(ModelMessage::tool_result(
                expected.tool_call_id.clone(),
                content.clone(),
            ));
        }
        Self::prepare_unit(CompactionUnitKind::ToolExchange, messages)
    }

    fn prepare_unit(
        kind: CompactionUnitKind,
        messages: Vec<ModelMessage>,
    ) -> Result<PreparedLiveCompactionUnit, LiveConversationError> {
        PreparedLiveCompactionUnit::for_live_reducer(kind, messages.into()).map_err(|_| {
            LiveConversationError::new(LiveConversationErrorReason::InvalidCompactionSource)
        })
    }

    #[cfg(test)]
    fn test_prepared_unit_preflight(&self) -> Result<(), LiveConversationError> {
        if self.fail_prepared_unit_preflight {
            Err(LiveConversationError::new(
                LiveConversationErrorReason::InvalidCompactionSource,
            ))
        } else {
            Ok(())
        }
    }

    fn stored_request_body(request: &InteractionRequest) -> StoredInteractionRequestBody {
        match request {
            InteractionRequest::ToolApproval(value) => {
                StoredInteractionRequestBody::ToolApproval(value.view().clone())
            }
            InteractionRequest::UserQuestion(value) => {
                StoredInteractionRequestBody::UserQuestion(value.clone())
            }
        }
    }

    fn stored_resolution(
        request_id: RequestId,
        item_id: ItemId,
        request: &InteractionRequest,
        resolution: &ResolvedInteraction,
        resolution_key: Option<InteractionResolutionKey>,
    ) -> Result<StoredInteractionResolution, LiveConversationError> {
        let body = match (request, resolution.live(), resolution.view().as_ref()) {
            (
                InteractionRequest::ToolApproval(_),
                InteractionResolution::ToolApproval(_),
                InteractionResolutionViewRef::ToolApproval(value),
            ) => StoredInteractionResolutionBody::ToolApproval(*value),
            (
                InteractionRequest::UserQuestion(_),
                InteractionResolution::UserAnswer(_),
                InteractionResolutionViewRef::UserAnswer(value),
            ) => StoredInteractionResolutionBody::UserAnswer(value.clone()),
            (
                InteractionRequest::ToolApproval(_) | InteractionRequest::UserQuestion(_),
                InteractionResolution::Cancelled(reason),
                InteractionResolutionViewRef::Cancelled {
                    reason: view_reason,
                },
            ) if reason == &view_reason => StoredInteractionResolutionBody::Cancelled(*reason),
            _ => {
                return Err(LiveConversationError::new(
                    LiveConversationErrorReason::InteractionConflict,
                ));
            }
        };
        StoredInteractionResolution::reconstruct(request_id, item_id, body, resolution_key).map_err(
            |_| LiveConversationError::new(LiveConversationErrorReason::InteractionConflict),
        )
    }

    fn is_started_tool_invocation(&self, item_id: ItemId, turn_id: TurnId) -> bool {
        self.relations.iter().any(|relation| {
            relation.item_id() == item_id
                && relation.turn_id() == turn_id
                && relation.family() == ItemContentFamily::ToolInvocation
        }) && self.tool_exchange.as_ref().is_some_and(|exchange| {
            exchange
                .expected
                .iter()
                .any(|expected| expected.item_id == item_id && expected.terminal.is_none())
        })
    }

    fn has_pending_interaction_for_item(&self, item_id: ItemId) -> bool {
        self.interactions.iter().any(|interaction| {
            interaction.item_id == item_id && matches!(interaction.state, InteractionState::Pending)
        })
    }

    fn flatten_stable_messages(&self) -> Vec<ModelMessage> {
        self.stable_units
            .iter()
            .flat_map(|unit| unit.messages().iter().cloned())
            .collect()
    }

    fn fresh_compaction_source(
        &self,
    ) -> Result<Arc<LiveCompactionSourceView>, LiveConversationError> {
        LiveCompactionSourceView::for_live_reducer(
            self.session_id,
            self.revision,
            self.stable_units.clone().into(),
        )
        .map(Arc::new)
        .map_err(|_| {
            LiveConversationError::new(LiveConversationErrorReason::InvalidCompactionSource)
        })
    }

    fn allocate_entry_id(&mut self) -> Result<EntryId, LiveConversationError> {
        #[cfg(test)]
        {
            let (entry_ids, scripted) = (&mut self.entry_ids, &mut self.scripted_entry_ids);
            if let Some(scripted) = scripted {
                return entry_ids
                    .allocate_candidates(|| {
                        scripted.allocation_calls += 1;
                        scripted.candidates.pop_front().unwrap_or(Err(()))
                    })
                    .map_err(|_| {
                        LiveConversationError::new(LiveConversationErrorReason::EntryIdAllocation)
                    });
            }
        }
        self.entry_ids
            .allocate()
            .map_err(|_| LiveConversationError::new(LiveConversationErrorReason::EntryIdAllocation))
    }

    fn make_entry(
        &self,
        entry_id: EntryId,
        turn_id: TurnId,
        timestamp: Timestamp,
        body: StoredEntryBody,
    ) -> Arc<StoredSessionEntry> {
        Arc::new(StoredSessionEntry::reconstruct(
            entry_id,
            self.selected_path.last().map(|entry| entry.entry_id()),
            self.session_id,
            turn_id,
            timestamp,
            body,
        ))
    }

    /// Installs deterministic entropy candidates for reducer tests without changing the
    /// production allocation path or exposing a production injection abstraction.
    #[cfg(test)]
    pub(crate) fn script_entry_id_candidates(
        &mut self,
        candidates: impl IntoIterator<Item = Result<EntryId, ()>>,
    ) {
        self.scripted_entry_ids = Some(ScriptedEntryIdCandidates {
            candidates: candidates.into_iter().collect(),
            allocation_calls: 0,
        });
    }

    #[cfg(test)]
    pub(crate) fn clear_scripted_entry_id_candidates(&mut self) {
        self.scripted_entry_ids = None;
    }

    #[cfg(test)]
    pub(crate) fn set_prepared_unit_failure_for_test(&mut self, enabled: bool) {
        self.fail_prepared_unit_preflight = enabled;
    }

    #[cfg(test)]
    pub(crate) fn scripted_allocation_calls(&self) -> usize {
        self.scripted_entry_ids
            .as_ref()
            .map_or(0, |scripted| scripted.allocation_calls)
    }

    #[cfg(test)]
    pub(crate) fn entry_id_is_reserved_for_test(&self, entry_id: EntryId) -> bool {
        self.entry_ids.reserved.contains(&entry_id)
    }

    /// Exact test-only owner clone for acceptance probes. Production state deliberately remains
    /// non-`Clone` because it owns reducer authority and real entropy.
    #[cfg(test)]
    pub(crate) fn clone_for_test(&self) -> Self {
        Self {
            session_id: self.session_id,
            selected_path: self.selected_path.clone(),
            entry_ids: EntryIdGenerator::new(self.entry_ids.reserved.iter().copied()),
            revision: self.revision,
            relations: self.relations.clone(),
            interactions: self
                .interactions
                .iter()
                .map(|interaction| Interaction {
                    request_id: interaction.request_id,
                    turn_id: interaction.turn_id,
                    item_id: interaction.item_id,
                    request: interaction.request.clone(),
                    state: match &interaction.state {
                        InteractionState::Pending => InteractionState::Pending,
                        InteractionState::Resolved {
                            resolution,
                            resolution_key,
                        } => InteractionState::Resolved {
                            resolution: resolution.clone_for_test(),
                            resolution_key: resolution_key.clone(),
                        },
                    },
                })
                .collect(),
            stable_units: self.stable_units.clone(),
            current_turn: self.current_turn,
            tool_exchange: self.tool_exchange.clone(),
            scripted_entry_ids: self.scripted_entry_ids.clone(),
            fail_prepared_unit_preflight: self.fail_prepared_unit_preflight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_session_lifecycle::{AgentRevisionRef, ForkAnchor};
    use crate::compaction::{MAX_STORED_COMPACTION_SUMMARY_BYTES, StoredCompaction};
    use crate::model_gateway::{
        ModelFinishReason, ModelId, ModelReasoningSummary, ModelResponseSummary, ModelServiceClass,
        ProviderId, ProviderResponseMetadata,
    };
    use crate::prompt::{CanonicalUserMessage, MessageContent, MessageRecord, ModelMessageRef};
    use crate::tools::{
        ToolAbandonReason, ToolApprovalDecisionInput, ToolOutcomeSource, ToolResultContent,
        ToolResultDisposition, UserQuestionAnswer, UserQuestionField, UserQuestionFieldAnswer,
        UserQuestionInput, UserQuestionRequest,
    };
    use crate::turn_item_interaction::InteractionResolutionInput;

    fn entry_id(value: &str) -> EntryId {
        value.parse().expect("test entry IDs are valid")
    }

    fn entry(number: u8) -> EntryId {
        format!("ent_{number:032x}")
            .parse()
            .expect("test entry IDs are valid")
    }

    fn session(number: u8) -> SessionId {
        format!("ses_{number:032x}")
            .parse()
            .expect("test session IDs are valid")
    }

    fn turn(number: u8) -> TurnId {
        format!("trn_{number:032x}")
            .parse()
            .expect("test turn IDs are valid")
    }

    fn item(number: u8) -> ItemId {
        format!("itm_{number:032x}")
            .parse()
            .expect("test item IDs are valid")
    }

    fn request(number: u8) -> RequestId {
        format!("req_{number:032x}")
            .parse()
            .expect("test request IDs are valid")
    }

    fn resolution_key(number: u8) -> InteractionResolutionKey {
        format!("irk_{number:032x}")
            .parse()
            .expect("test interaction keys are valid")
    }

    fn timestamp() -> Timestamp {
        "2026-07-31T12:00:00.000Z"
            .parse()
            .expect("test timestamps are valid")
    }

    fn fork_header(child_session_id: SessionId) -> crate::conversation_storage::SessionHeader {
        crate::conversation_storage::SessionHeader::reconstruct(
            1,
            child_session_id,
            timestamp(),
            AgentRevisionRef::new(
                "agt_11111111111111111111111111111111".parse().unwrap(),
                "ar_1".parse().unwrap(),
            ),
            "sdr_1".parse().unwrap(),
        )
    }

    fn model() -> ModelResponseSummary {
        ModelResponseSummary::reconstruct(
            "fixture".parse::<ProviderId>().unwrap(),
            "scripted".parse::<ModelId>().unwrap(),
            ModelReasoningSummary::Disabled,
            ModelServiceClass::Standard,
        )
    }

    fn metadata() -> ProviderResponseMetadata {
        ProviderResponseMetadata::reconstruct(None, None, None)
    }

    fn user_message(item_id: ItemId, source: UserMessageSource, text: &str) -> StoredUserMessage {
        let content = CanonicalUserMessage::reconstruct(
            MessageRecord::reconstruct(vec![MessageContent::reconstruct_text(text).unwrap()])
                .unwrap(),
            vec![],
        )
        .unwrap();
        StoredUserMessage::reconstruct(item_id, source, content)
    }

    fn assistant_text(item_id: ItemId, text: &str) -> StoredAssistantMessage {
        StoredAssistantMessage::reconstruct(
            crate::turn_item_interaction::AssistantDisposition::Final,
            vec![StoredAssistantContent::Text {
                item_id,
                text: Arc::from(text),
            }],
            model(),
            None,
            ModelFinishReason::Stop,
            std::num::NonZeroU32::new(1).unwrap(),
            None,
            0,
            metadata(),
        )
        .unwrap()
    }

    fn assistant_with_calls(calls: &[(ItemId, &str)]) -> StoredAssistantMessage {
        StoredAssistantMessage::reconstruct(
            crate::turn_item_interaction::AssistantDisposition::Intermediate,
            calls
                .iter()
                .map(|(item_id, call)| StoredAssistantContent::ToolCall {
                    item_id: *item_id,
                    tool_call_id: (*call).parse().unwrap(),
                    name: "test_tool".parse().unwrap(),
                    arguments: crate::wire::BoundedJsonObject::from_slice(br"{}").unwrap(),
                })
                .collect(),
            model(),
            None,
            ModelFinishReason::ToolCalls,
            std::num::NonZeroU32::new(1).unwrap(),
            None,
            0,
            metadata(),
        )
        .unwrap()
    }

    fn completed_tool(item_id: ItemId, call: &str, text: &str) -> StoredToolMessage {
        StoredToolMessage::reconstruct(
            item_id,
            call.parse().unwrap(),
            StoredToolOutcome::completed(
                ToolOutcomeSource::PreExecution,
                ToolResultDisposition::Succeeded,
                ToolResultContent::from_text_parts(vec![text.to_owned()]).unwrap(),
            )
            .unwrap(),
        )
    }

    fn abandoned_tool(item_id: ItemId, call: &str) -> StoredToolMessage {
        StoredToolMessage::reconstruct(
            item_id,
            call.parse().unwrap(),
            StoredToolOutcome::Abandoned {
                reason: ToolAbandonReason::OutcomeUnknown,
            },
        )
    }

    fn scripted_state(
        session_id: SessionId,
        candidates: impl IntoIterator<Item = EntryId>,
    ) -> LiveSessionState {
        let mut state = LiveSessionState::new(session_id, []);
        state.script_entry_id_candidates(candidates.into_iter().map(Ok::<_, ()>));
        state
    }

    fn start(state: &mut LiveSessionState, turn_id: TurnId) -> AppliedConversationFact {
        state
            .apply_user_message(
                user_message(item(1), UserMessageSource::Input, "input"),
                turn_id,
                timestamp(),
            )
            .unwrap()
    }

    #[test]
    fn live_fork_capture_resolves_every_public_message_anchor_on_one_snapshot() {
        let source_session_id = session(1);
        let child_session_id = session(2);
        let turn_id = turn(1);
        let mut state = scripted_state(source_session_id, [entry(1), entry(2)]);
        start(&mut state, turn_id);
        state
            .complete_with_assistant_message(assistant_text(item(2), "final"), turn_id, timestamp())
            .unwrap();
        let header = fork_header(child_session_id);

        for (anchor, expected_lines) in [
            (ForkAnchor::Genesis, 1),
            (ForkAnchor::BeforeUserMessage { item_id: item(1) }, 1),
            (ForkAnchor::AfterUserMessage { item_id: item(1) }, 2),
            (ForkAnchor::BeforeFinalAgentMessage { item_id: item(2) }, 2),
            (ForkAnchor::AfterFinalAgentMessage { item_id: item(2) }, 3),
        ] {
            let captured = state.capture_fork_conversation(anchor).unwrap();
            let mut encoded = Vec::new();
            captured.write_for_child(&header, &mut encoded).unwrap();
            assert_eq!(
                encoded
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .count(),
                expected_lines
            );
            assert!(
                !String::from_utf8(encoded)
                    .unwrap()
                    .contains(&source_session_id.to_string())
            );
        }
        assert_eq!(
            state
                .capture_fork_conversation(ForkAnchor::BeforeFinalAgentMessage { item_id: item(1) })
                .unwrap_err(),
            crate::conversation_storage::ForkAnchorResolutionError::InvalidAnchor
        );
    }

    fn assert_exact_fact_arc(state: &LiveSessionState, fact: &AppliedConversationFact) {
        assert!(Arc::ptr_eq(
            state.selected_path.last().expect("fact was appended"),
            fact.entry(),
        ));
    }

    fn live_error<T>(result: Result<T, LiveConversationError>) -> LiveConversationError {
        match result {
            Ok(_) => panic!("live reducer operation unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    fn approval_request() -> InteractionRequest {
        InteractionRequest::tool_approval(crate::tools::live_approval_request_fixture())
    }

    fn question_request() -> InteractionRequest {
        InteractionRequest::user_question(
            UserQuestionRequest::reconstruct(
                None,
                vec![
                    UserQuestionField::reconstruct(
                        0,
                        "Continue?",
                        false,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        )
    }

    fn pending_approval_state() -> LiveSessionState {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=8).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request(1), item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state
    }

    fn approval_denial() -> ResolvedInteraction {
        approval_request()
            .resolve_host(InteractionResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Deny,
            ))
            .unwrap()
    }

    fn approval_allowance() -> ResolvedInteraction {
        approval_request()
            .resolve_host(InteractionResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Allow { option_index: 0 },
            ))
            .unwrap()
    }

    #[test]
    fn host_interaction_resolution_has_typed_target_family_and_idempotency_errors() {
        let mut state = pending_approval_state();
        let answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "answer").unwrap()])
                .unwrap();
        for (request_id, expected_turn_id, item_id, input, expected) in [
            (
                request(2),
                turn(1),
                item(2),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                HostInteractionResolutionError::NotFound,
            ),
            (
                request(1),
                turn(2),
                item(2),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                HostInteractionResolutionError::ExpectedTurnMismatch,
            ),
            (
                request(1),
                turn(1),
                item(3),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                HostInteractionResolutionError::NotFound,
            ),
            (
                request(1),
                turn(1),
                item(2),
                InteractionResolutionInput::UserAnswer(answer),
                HostInteractionResolutionError::FamilyMismatch,
            ),
            (
                request(1),
                turn(1),
                item(2),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Allow {
                    option_index: u32::MAX,
                }),
                HostInteractionResolutionError::InvalidResolution,
            ),
        ] {
            assert!(matches!(
                state.apply_host_interaction_resolution(
                    request_id,
                    expected_turn_id,
                    item_id,
                    resolution_key(1),
                    input,
                    timestamp(),
                ),
                Err(error) if error == expected
            ));
        }

        let mut allocation_failure = pending_approval_state();
        allocation_failure.script_entry_id_candidates([Err(())]);
        assert!(matches!(
            allocation_failure.apply_host_interaction_resolution(
                request(1),
                turn(1),
                item(2),
                resolution_key(1),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                timestamp(),
            ),
            Err(HostInteractionResolutionError::Internal)
        ));

        assert!(matches!(
            state.apply_host_interaction_resolution(
                request(1),
                turn(1),
                item(2),
                resolution_key(1),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                timestamp(),
            ),
            Ok(HostInteractionResolutionApplyOutcome::Applied { .. })
        ));
        assert!(matches!(
            state.apply_host_interaction_resolution(
                request(1),
                turn(1),
                item(2),
                resolution_key(1),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                timestamp(),
            ),
            Ok(HostInteractionResolutionApplyOutcome::Idempotent)
        ));
        assert!(matches!(
            state.apply_host_interaction_resolution(
                request(1),
                turn(1),
                item(2),
                resolution_key(1),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Allow {
                    option_index: 0,
                }),
                timestamp(),
            ),
            Err(HostInteractionResolutionError::CommandConflict)
        ));
        assert!(matches!(
            state.apply_host_interaction_resolution(
                request(1),
                turn(1),
                item(2),
                resolution_key(2),
                InteractionResolutionInput::ToolApproval(ToolApprovalDecisionInput::Deny),
                timestamp(),
            ),
            Err(HostInteractionResolutionError::AlreadyResolved)
        ));
    }

    fn owner_cancellation() -> InteractionCancelReason {
        InteractionCancelReason::TurnCancelled
    }

    fn answer_for_different_question() -> ResolvedInteraction {
        let request = InteractionRequest::user_question(
            UserQuestionRequest::reconstruct(
                None,
                vec![
                    UserQuestionField::reconstruct(
                        1,
                        "A different question",
                        true,
                        UserQuestionInput::Text { multiline: false },
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        request
            .resolve_host(InteractionResolutionInput::UserAnswer(
                UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(1, "answer").unwrap()])
                    .unwrap(),
            ))
            .unwrap()
    }

    /// Test-only complete reducer-state capture. Scripts are included for pre-allocation
    /// failures; allocation failure tests explicitly omit their intentionally consumed script.
    struct StateSnapshot {
        session_id: SessionId,
        selected_head: Option<EntryId>,
        path: Vec<(EntryId, usize)>,
        reserved_entry_ids: BTreeSet<EntryId>,
        revision: ConversationRevision,
        current_turn: Option<TurnId>,
        relations: Vec<ItemRelation>,
        interactions: Vec<InteractionSnapshot>,
        stable_units: Vec<StableUnitSnapshot>,
        tool_exchange: Option<PendingToolExchangeSnapshot>,
        scripted_entry_ids: Option<ScriptedEntryIdCandidates>,
        fail_prepared_unit_preflight: bool,
    }

    struct InteractionSnapshot {
        request_id: RequestId,
        turn_id: TurnId,
        item_id: ItemId,
        request: InteractionRequest,
        state: InteractionStateSnapshot,
    }

    enum InteractionStateSnapshot {
        Pending,
        Resolved {
            resolution: ResolvedInteraction,
            resolution_view: crate::turn_item_interaction::InteractionResolutionView,
            resolution_key: Option<InteractionResolutionKey>,
        },
    }

    struct StableUnitSnapshot {
        first_entry_id: EntryId,
        kind: CompactionUnitKind,
        message_identities: Vec<usize>,
    }

    struct PendingToolExchangeSnapshot {
        assistant_entry_id: EntryId,
        assistant_message: ModelMessage,
        assistant_message_identity: usize,
        expected: Vec<ExpectedToolCallSnapshot>,
    }

    struct ExpectedToolCallSnapshot {
        item_id: ItemId,
        tool_call_id: crate::tools::ToolCallId,
        terminal: Option<StoredToolMessage>,
    }

    impl StateSnapshot {
        fn capture(state: &LiveSessionState) -> Self {
            let interactions = state
                .interactions
                .iter()
                .map(|interaction| InteractionSnapshot {
                    request_id: interaction.request_id,
                    turn_id: interaction.turn_id,
                    item_id: interaction.item_id,
                    request: interaction.request.clone(),
                    state: match &interaction.state {
                        InteractionState::Pending => InteractionStateSnapshot::Pending,
                        InteractionState::Resolved {
                            resolution,
                            resolution_key,
                        } => InteractionStateSnapshot::Resolved {
                            resolution: resolution.clone_for_test(),
                            resolution_view: resolution.view().clone(),
                            resolution_key: resolution_key.clone(),
                        },
                    },
                })
                .collect();
            let stable_units = state
                .stable_units
                .iter()
                .map(|unit| StableUnitSnapshot {
                    first_entry_id: *unit.first_entry_id(),
                    kind: unit.kind(),
                    message_identities: unit
                        .messages()
                        .iter()
                        .map(|message| std::ptr::from_ref(message) as usize)
                        .collect(),
                })
                .collect();
            let tool_exchange =
                state
                    .tool_exchange
                    .as_ref()
                    .map(|exchange| PendingToolExchangeSnapshot {
                        assistant_entry_id: exchange.assistant_entry_id,
                        assistant_message: exchange.assistant_message.clone(),
                        assistant_message_identity: std::ptr::from_ref(&exchange.assistant_message)
                            as usize,
                        expected: exchange
                            .expected
                            .iter()
                            .map(|expected| ExpectedToolCallSnapshot {
                                item_id: expected.item_id,
                                tool_call_id: expected.tool_call_id.clone(),
                                terminal: expected.terminal.clone(),
                            })
                            .collect(),
                    });
            Self {
                session_id: state.session_id,
                selected_head: state.selected_path.last().map(|entry| entry.entry_id()),
                path: state
                    .selected_path
                    .iter()
                    .map(|entry| (entry.entry_id(), Arc::as_ptr(entry) as usize))
                    .collect(),
                reserved_entry_ids: state.entry_ids.reserved.clone(),
                revision: state.revision,
                current_turn: state.current_turn,
                relations: state.relations.clone(),
                interactions,
                stable_units,
                tool_exchange,
                scripted_entry_ids: state.scripted_entry_ids.clone(),
                fail_prepared_unit_preflight: state.fail_prepared_unit_preflight,
            }
        }

        fn assert_unchanged(&self, state: &LiveSessionState) {
            self.assert_matches(state, true, true);
        }

        fn assert_exact_clone(&self, state: &LiveSessionState) {
            self.assert_matches(state, false, true);
        }

        fn assert_unchanged_except_script(&self, state: &LiveSessionState) {
            self.assert_matches(state, true, false);
        }

        fn assert_matches(
            &self,
            state: &LiveSessionState,
            exact_exchange_identity: bool,
            include_script: bool,
        ) {
            assert_eq!(state.session_id, self.session_id);
            assert_eq!(
                state.selected_path.last().map(|entry| entry.entry_id()),
                self.selected_head
            );
            assert_eq!(
                state
                    .selected_path
                    .iter()
                    .map(|entry| (entry.entry_id(), Arc::as_ptr(entry) as usize))
                    .collect::<Vec<_>>(),
                self.path
            );
            assert_eq!(state.entry_ids.reserved, self.reserved_entry_ids);
            if include_script {
                assert!(state.scripted_entry_ids == self.scripted_entry_ids);
            }
            assert_eq!(
                state.fail_prepared_unit_preflight,
                self.fail_prepared_unit_preflight
            );
            assert_eq!(state.revision, self.revision);
            assert_eq!(state.current_turn, self.current_turn);
            assert_eq!(state.relations, self.relations);
            assert_eq!(state.interactions.len(), self.interactions.len());
            for (actual, expected) in state.interactions.iter().zip(&self.interactions) {
                assert_eq!(actual.request_id, expected.request_id);
                assert_eq!(actual.turn_id, expected.turn_id);
                assert_eq!(actual.item_id, expected.item_id);
                assert!(actual.request == expected.request);
                match (&actual.state, &expected.state) {
                    (InteractionState::Pending, InteractionStateSnapshot::Pending) => {}
                    (
                        InteractionState::Resolved {
                            resolution,
                            resolution_key,
                        },
                        InteractionStateSnapshot::Resolved {
                            resolution: expected_resolution,
                            resolution_view: expected_resolution_view,
                            resolution_key: expected_resolution_key,
                        },
                    ) => {
                        assert!(resolution.live() == expected_resolution.live());
                        assert_eq!(resolution.view(), expected_resolution_view);
                        assert!(resolution_key == expected_resolution_key);
                    }
                    _ => panic!("interaction state changed"),
                }
            }
            assert_eq!(state.stable_units.len(), self.stable_units.len());
            for (actual, expected) in state.stable_units.iter().zip(&self.stable_units) {
                assert_eq!(actual.first_entry_id(), &expected.first_entry_id);
                assert_eq!(actual.kind(), expected.kind);
                assert_eq!(
                    actual
                        .messages()
                        .iter()
                        .map(|message| std::ptr::from_ref(message) as usize)
                        .collect::<Vec<_>>(),
                    expected.message_identities
                );
            }
            match (&state.tool_exchange, &self.tool_exchange) {
                (None, None) => {}
                (Some(actual), Some(expected)) => {
                    assert_eq!(actual.assistant_entry_id, expected.assistant_entry_id);
                    assert_eq!(
                        actual.assistant_message.as_ref(),
                        expected.assistant_message.as_ref()
                    );
                    if exact_exchange_identity {
                        assert_eq!(
                            std::ptr::from_ref(&actual.assistant_message) as usize,
                            expected.assistant_message_identity
                        );
                    }
                    assert_eq!(actual.expected.len(), expected.expected.len());
                    for (actual, expected) in actual.expected.iter().zip(&expected.expected) {
                        assert_eq!(actual.item_id, expected.item_id);
                        assert_eq!(actual.tool_call_id, expected.tool_call_id);
                        assert_eq!(actual.terminal, expected.terminal);
                    }
                }
                _ => panic!("pending tool exchange changed"),
            }
        }
    }

    fn first_unreserved_scripted_sentinel(state: &LiveSessionState) -> EntryId {
        let scripted = state
            .scripted_entry_ids
            .as_ref()
            .expect("post-error probes require scripted entry IDs");
        for candidate in &scripted.candidates {
            match candidate {
                Ok(candidate) if !state.entry_ids.reserved.contains(candidate) => {
                    return *candidate;
                }
                Ok(_) => {}
                Err(()) => panic!("post-error probe encountered scripted entropy failure"),
            }
        }
        panic!("post-error probe has no unreserved scripted sentinel");
    }

    /// Proves that a no-ID outcome did not merely leave the scripted queue looking intact. The
    /// exact test clone must accept a real reducer fact at the current sentinel, selecting the
    /// first valid fact family from the live state rather than mutating test internals.
    fn assert_post_error_probe_uses_original_sentinel(state: &LiveSessionState) {
        assert!(
            !state.fail_prepared_unit_preflight,
            "disable the persistent test preflight failure before probing"
        );
        let expected_id = first_unreserved_scripted_sentinel(state);
        let snapshot = StateSnapshot::capture(state);
        let mut probe = state.clone_for_test();
        snapshot.assert_exact_clone(&probe);
        let calls_before = probe.scripted_allocation_calls();

        let applied = if let Some(request_id) = probe.interactions.iter().find_map(|interaction| {
            matches!(interaction.state, InteractionState::Pending).then_some(interaction.request_id)
        }) {
            match probe
                .apply_interaction_resolution(
                    InteractionResolutionCandidate::owner_cancellation(
                        request_id,
                        owner_cancellation(),
                    )
                    .expect("pending interaction accepts owner cancellation"),
                    timestamp(),
                )
                .expect("pending interaction probe applies")
            {
                InteractionResolutionApplyOutcome::Applied(fact) => fact,
                InteractionResolutionApplyOutcome::Idempotent { .. } => {
                    panic!("pending interaction probe unexpectedly became idempotent")
                }
            }
        } else if let Some((item_id, tool_call_id)) =
            probe.tool_exchange.as_ref().and_then(|exchange| {
                exchange
                    .expected
                    .iter()
                    .find(|expected| expected.terminal.is_none())
                    .map(|expected| (expected.item_id, expected.tool_call_id.clone()))
            })
        {
            let turn_id = probe
                .current_turn
                .expect("unfinished tool exchange has a current turn");
            probe
                .apply_tool_message(
                    completed_tool(item_id, tool_call_id.as_str(), "post-error probe"),
                    turn_id,
                    timestamp(),
                )
                .expect("unfinished tool probe applies")
        } else if let Some(turn_id) = probe.current_turn {
            assert!(
                !probe
                    .relations
                    .iter()
                    .any(|relation| relation.item_id() == item(250)),
                "probe item must remain fresh"
            );
            probe
                .apply_assistant_message(
                    assistant_text(item(250), "post-error probe"),
                    turn_id,
                    timestamp(),
                )
                .expect("current turn accepts a fresh assistant probe")
        } else {
            probe
                .apply_user_message(
                    user_message(item(250), UserMessageSource::Input, "post-error probe"),
                    turn(250),
                    timestamp(),
                )
                .expect("idle state accepts an input probe")
        };

        assert_exact_fact_arc(&probe, &applied);
        assert_eq!(applied.entry().entry_id(), expected_id);
        assert!(probe.scripted_allocation_calls() > calls_before);
        assert!(probe.entry_ids.reserved.contains(&expected_id));
    }

    fn assert_validation_rejection_unchanged<T>(
        state: &LiveSessionState,
        snapshot: &StateSnapshot,
        allocation_calls: usize,
        result: Result<T, LiveConversationError>,
        reason: LiveConversationErrorReason,
    ) {
        assert_eq!(live_error(result).reason(), reason);
        assert_eq!(state.scripted_allocation_calls(), allocation_calls);
        snapshot.assert_unchanged(state);
        assert_post_error_probe_uses_original_sentinel(state);
    }

    #[test]
    fn steer_rejects_every_unfinished_tool_exchange_before_projection_or_allocation() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=6).map(entry));

        let before_start = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_user_message(
            user_message(item(9), UserMessageSource::Steer, "not started"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &before_start,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidTurn,
        );

        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[
                    (item(2), "call_a"),
                    (item(3), "call_b"),
                    (item(4), "call_c"),
                ]),
                turn_id,
                timestamp(),
            )
            .unwrap();

        for phase in ["pending", "partial", "abandoned"] {
            if phase == "partial" {
                state
                    .apply_tool_message(
                        completed_tool(item(2), "call_a", "first"),
                        turn_id,
                        timestamp(),
                    )
                    .unwrap();
            }
            if phase == "abandoned" {
                state
                    .apply_tool_message(abandoned_tool(item(3), "call_b"), turn_id, timestamp())
                    .unwrap();
            }
            let snapshot = StateSnapshot::capture(&state);
            let calls_before = state.scripted_allocation_calls();
            let result = state.apply_user_message(
                user_message(item(9), UserMessageSource::Steer, phase),
                turn_id,
                timestamp(),
            );
            assert_validation_rejection_unchanged(
                &state,
                &snapshot,
                calls_before,
                result,
                LiveConversationErrorReason::PendingToolExchange,
            );
        }

        // Input has its independent exact start rule and therefore remains InvalidTurn even
        // while an unfinished exchange happens to be present.
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_user_message(
            user_message(item(10), UserMessageSource::Input, "second input"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidTurn,
        );
    }

    #[test]
    fn pending_interaction_blocks_its_tool_terminal_until_the_owner_settles_it() {
        let turn_id = turn(1);
        let request_id = request(1);
        let mut state = scripted_state(session(1), (1..=6).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();

        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_tool_message(
            completed_tool(item(2), "call_a", "must wait"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InteractionConflict,
        );

        assert!(matches!(
            state
                .apply_interaction_resolution(
                    InteractionResolutionCandidate::owner_cancellation(
                        request_id,
                        owner_cancellation(),
                    )
                    .unwrap(),
                    timestamp(),
                )
                .unwrap(),
            InteractionResolutionApplyOutcome::Applied(_)
        ));
        let settled = state
            .apply_tool_message(
                completed_tool(item(2), "call_a", "may settle"),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &settled);
        assert_eq!(settled.entry().entry_id(), entry(5));
        assert_eq!(settled.revision().0, 3);
    }

    #[test]
    fn exact_interaction_binding_preserves_stored_facts_and_host_only_idempotence() {
        let turn_id = turn(1);
        let request_id = request(1);
        let key = resolution_key(1);
        let mut state = scripted_state(session(1), (1..=8).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();

        let exact_request = approval_request();
        let InteractionRequestView::ToolApproval(expected_request_view) = exact_request.view()
        else {
            panic!("approval fixture must have an approval view");
        };
        let requested = state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(2), exact_request),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &requested);
        let StoredEntryBody::InteractionRequested(stored_request) = requested.entry().body() else {
            panic!("interaction request must retain its exact stored body");
        };
        assert_eq!(stored_request.request_id(), request_id);
        assert_eq!(stored_request.item_id(), item(2));
        assert!(matches!(
            stored_request.request(),
            StoredInteractionRequestBody::ToolApproval(view) if view == &expected_request_view
        ));

        let applied = state
            .apply_interaction_resolution(
                InteractionResolutionCandidate::host(request_id, key.clone(), approval_denial())
                    .unwrap(),
                timestamp(),
            )
            .unwrap();
        let InteractionResolutionApplyOutcome::Applied(resolved) = applied else {
            panic!("first host resolution must append a fact");
        };
        assert_exact_fact_arc(&state, &resolved);
        let StoredEntryBody::InteractionResolved(stored_resolution) = resolved.entry().body()
        else {
            panic!("interaction resolution must retain its exact stored body");
        };
        assert_eq!(stored_resolution.request_id(), request_id);
        assert_eq!(stored_resolution.item_id(), item(2));
        assert!(stored_resolution.resolution_key() == Some(&key));
        assert!(matches!(
            stored_resolution.resolution(),
            StoredInteractionResolutionBody::ToolApproval(value)
                if matches!(value.as_ref(), crate::tools::ToolApprovalResolutionRef::Denied)
        ));

        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let idempotent = state.apply_interaction_resolution(
            InteractionResolutionCandidate::host(request_id, key.clone(), approval_denial())
                .unwrap(),
            timestamp(),
        );
        assert!(matches!(
            idempotent.unwrap(),
            InteractionResolutionApplyOutcome::Idempotent { revision }
                if revision == ConversationRevision(2)
        ));
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);

        for (candidate_key, candidate_resolution) in [
            (key.clone(), approval_allowance()),
            (resolution_key(2), approval_denial()),
        ] {
            let snapshot = StateSnapshot::capture(&state);
            let calls_before = state.scripted_allocation_calls();
            let result = state.apply_interaction_resolution(
                InteractionResolutionCandidate::host(
                    request_id,
                    candidate_key,
                    candidate_resolution,
                )
                .unwrap(),
                timestamp(),
            );
            assert_validation_rejection_unchanged(
                &state,
                &snapshot,
                calls_before,
                result,
                LiveConversationErrorReason::InteractionConflict,
            );
        }

        let mut owner_cancelled = scripted_state(session(2), (1..=6).map(entry));
        start(&mut owner_cancelled, turn_id);
        owner_cancelled
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        owner_cancelled
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        owner_cancelled
            .apply_interaction_resolution(
                InteractionResolutionCandidate::owner_cancellation(
                    request_id,
                    owner_cancellation(),
                )
                .unwrap(),
                timestamp(),
            )
            .unwrap();
        let snapshot = StateSnapshot::capture(&owner_cancelled);
        let calls_before = owner_cancelled.scripted_allocation_calls();
        let result = owner_cancelled.apply_interaction_resolution(
            InteractionResolutionCandidate::owner_cancellation(request_id, owner_cancellation())
                .unwrap(),
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &owner_cancelled,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InteractionConflict,
        );
    }

    #[test]
    fn exact_question_resolution_and_invalid_input_bodies_are_preallocation_rejections() {
        let turn_id = turn(1);
        let request_id = request(1);
        let mut state = scripted_state(session(1), (1..=6).map(entry));

        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_user_message(
            user_message(item(1), UserMessageSource::Steer, "not started"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidTurn,
        );

        start(&mut state, turn_id);
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_user_message(
            user_message(item(2), UserMessageSource::Input, "second input"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidTurn,
        );

        let duplicate_relation = state.apply_user_message(
            user_message(item(1), UserMessageSource::Steer, "duplicate item"),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            duplicate_relation,
            LiveConversationErrorReason::InvalidRelation,
        );

        let mut malformed = assistant_text(item(2), "valid before corruption");
        malformed.content = vec![StoredAssistantContent::Text {
            item_id: item(2),
            text: Arc::from(""),
        }]
        .into();
        let malformed_body = state.apply_assistant_message(malformed, turn_id, timestamp());
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            malformed_body,
            LiveConversationErrorReason::InvalidRelation,
        );

        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let exact_question = question_request();
        let InteractionRequestView::UserQuestion(expected_question_view) = exact_question.view()
        else {
            panic!("question fixture must have a question view");
        };
        let requested = state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(2), exact_question),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &requested);
        let StoredEntryBody::InteractionRequested(stored_request) = requested.entry().body() else {
            panic!("question request must retain its exact stored body");
        };
        assert_eq!(stored_request.request_id(), request_id);
        assert_eq!(stored_request.item_id(), item(2));
        assert!(matches!(
            stored_request.request(),
            StoredInteractionRequestBody::UserQuestion(view) if view == &expected_question_view
        ));
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_interaction_resolution(
            InteractionResolutionCandidate::host(
                request_id,
                resolution_key(1),
                answer_for_different_question(),
            )
            .unwrap(),
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InteractionConflict,
        );

        let expected_answer = UserQuestionAnswer::new(vec![
            UserQuestionFieldAnswer::text(0, "valid answer").unwrap(),
        ])
        .unwrap();
        let valid_resolution = question_request()
            .resolve_host(InteractionResolutionInput::UserAnswer(
                expected_answer.clone(),
            ))
            .unwrap();
        let next = state
            .apply_interaction_resolution(
                InteractionResolutionCandidate::host(
                    request_id,
                    resolution_key(1),
                    valid_resolution,
                )
                .unwrap(),
                timestamp(),
            )
            .unwrap();
        let InteractionResolutionApplyOutcome::Applied(resolved) = next else {
            panic!("first question resolution must append a fact");
        };
        assert_exact_fact_arc(&state, &resolved);
        let StoredEntryBody::InteractionResolved(stored_resolution) = resolved.entry().body()
        else {
            panic!("question resolution must retain its exact stored body");
        };
        assert_eq!(stored_resolution.request_id(), request_id);
        assert_eq!(stored_resolution.item_id(), item(2));
        assert!(stored_resolution.resolution_key() == Some(&resolution_key(1)));
        assert!(matches!(
            stored_resolution.resolution(),
            StoredInteractionResolutionBody::UserAnswer(answer) if answer == &expected_answer
        ));
    }

    #[test]
    fn zero_cut_and_wrong_resolution_origins_never_enter_the_reducer() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), [entry(1), entry(2)]);
        start(&mut state, turn_id);
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();

        assert!(NonZeroUsize::new(0).is_none());
        let owner_error = InteractionResolutionCandidate::owner_cancellation(
            request(1),
            InteractionCancelReason::HostCancelled,
        )
        .unwrap_err();
        assert_eq!(
            owner_error.reason,
            InteractionCandidateErrorReason::InvalidResolutionOrigin
        );
        let host_error = InteractionResolutionCandidate::host(
            request(1),
            resolution_key(1),
            ResolvedInteraction::cancelled_by_owner(owner_cancellation()).unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            host_error.reason,
            InteractionCandidateErrorReason::InvalidResolutionOrigin
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);
    }

    #[test]
    fn orphan_tool_result_never_allocates_and_leaves_its_scripted_sentinel_recordable() {
        let turn_id = turn(1);
        let sentinel = entry(2);
        let mut state = scripted_state(session(1), [entry(1), sentinel]);
        start(&mut state, turn_id);
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();

        let result = state.apply_tool_message(
            completed_tool(item(2), "orphan_call", "orphan result"),
            turn_id,
            timestamp(),
        );

        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidRelation,
        );
    }

    #[test]
    fn prepared_unit_failure_injection_is_persistent_preallocation_and_releases_the_same_sentinel()
    {
        let sentinel = entry(1);
        let mut state = scripted_state(session(1), [sentinel]);
        state.set_prepared_unit_failure_for_test(true);
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();

        for attempt in 0..2 {
            let error = live_error(state.apply_user_message(
                user_message(item(1), UserMessageSource::Input, "input"),
                turn(1),
                timestamp(),
            ));
            assert_eq!(
                error.reason(),
                LiveConversationErrorReason::InvalidCompactionSource,
                "preflight attempt {attempt}"
            );
            assert_eq!(state.scripted_allocation_calls(), calls_before);
            snapshot.assert_unchanged(&state);
        }

        state.set_prepared_unit_failure_for_test(false);
        assert_post_error_probe_uses_original_sentinel(&state);
    }

    #[test]
    fn valid_stored_assistant_bodies_make_prompt_projection_infallible() {
        // `StoredAssistantMessage::reconstruct`/wire validation closes every constructor error
        // that Prompt's assistant projection could otherwise report. M4 therefore deliberately
        // does not forge a malformed production body merely to reach InvalidPromptProjection.
        for body in [
            assistant_text(item(2), "text"),
            assistant_with_calls(&[(item(3), "call_a")]),
        ] {
            assert!(
                LiveSessionState::validate_stored_body(&StoredEntryBody::AssistantMessage(
                    body.clone()
                ))
                .is_ok()
            );
            assert!(LiveSessionState::project_assistant(&body, turn(1)).is_ok());
        }
    }

    #[test]
    fn ordinary_facts_bind_session_parent_turn_and_the_exact_returned_arc() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), [entry(1), entry(2), entry(3)]);

        let input = start(&mut state, turn_id);
        assert_exact_fact_arc(&state, &input);
        assert_eq!(input.entry().session_id(), session(1));
        assert_eq!(input.entry().parent_id(), None);
        assert_eq!(input.entry().turn_id(), turn_id);
        assert_eq!(input.revision().0, 1);

        let assistant = state
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        assert_exact_fact_arc(&state, &assistant);
        assert_eq!(assistant.entry().session_id(), session(1));
        assert_eq!(assistant.entry().parent_id(), Some(entry(1)));
        assert_eq!(assistant.entry().turn_id(), turn_id);
        assert_eq!(assistant.revision().0, 2);

        let views = state.capture_conversation_views().unwrap();
        assert_eq!(views.conversation().revision().0, 2);
        assert_eq!(views.compaction_source().revision().0, 2);
        assert_eq!(views.selected_head(), Some(&entry(2)));
        assert_eq!(views.relations().len(), 2);
        assert!(views.pending_interactions().is_empty());
        assert_eq!(views.conversation().messages().len(), 2);
        let clone = views.conversation().clone();
        assert_eq!(
            clone.messages().as_ptr(),
            views.conversation().messages().as_ptr()
        );
    }

    #[test]
    fn tool_results_promote_once_in_assistant_call_order_for_every_completion_order() {
        let orders = [
            [0_usize, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let calls = [
            (item(2), "call_a"),
            (item(3), "call_b"),
            (item(4), "call_c"),
        ];

        for order in orders {
            let turn_id = turn(1);
            let mut state = scripted_state(session(1), (1..=5).map(entry));
            assert_eq!(start(&mut state, turn_id).revision().0, 1);
            let assistant = state
                .apply_assistant_message(assistant_with_calls(&calls), turn_id, timestamp())
                .unwrap();
            assert_eq!(assistant.revision().0, 2);

            for (completion_index, call_index) in order.into_iter().enumerate() {
                let (item_id, call) = calls[call_index];
                let applied = state
                    .apply_tool_message(completed_tool(item_id, call, call), turn_id, timestamp())
                    .unwrap();
                assert_exact_fact_arc(&state, &applied);
                assert_eq!(
                    applied.revision().0,
                    if completion_index == 2 { 3 } else { 2 }
                );
            }

            let views = state.capture_conversation_views().unwrap();
            assert_eq!(views.conversation().messages().len(), 5);
            assert_eq!(views.compaction_source().units().len(), 2);
            assert_eq!(
                views.compaction_source().units()[1].kind(),
                CompactionUnitKind::ToolExchange
            );
            assert_eq!(
                views.compaction_source().units()[1].first_entry_id(),
                &entry(2)
            );
            let visible_calls = views.conversation().messages()[2..]
                .iter()
                .map(|message| match message.as_ref() {
                    ModelMessageRef::Tool { tool_call_id, .. } => tool_call_id.as_str(),
                    _ => panic!("complete exchange must contain only tool results after assistant"),
                })
                .collect::<Vec<_>>();
            assert_eq!(visible_calls, ["call_a", "call_b", "call_c"]);
            assert_eq!(state.revision.0, 3);
        }
    }

    fn generated_permutations(count: usize) -> Vec<Vec<usize>> {
        fn generate(prefix: &mut Vec<usize>, count: usize, output: &mut Vec<Vec<usize>>) {
            if prefix.len() == count {
                output.push(prefix.clone());
                return;
            }
            for next in 0..count {
                if !prefix.contains(&next) {
                    prefix.push(next);
                    generate(prefix, count, output);
                    prefix.pop();
                }
            }
        }

        let mut output = Vec::new();
        generate(&mut Vec::new(), count, &mut output);
        output
    }

    #[test]
    fn generated_tool_completion_permutations_preserve_assistant_order_for_one_through_four_calls()
    {
        let all_calls = [
            (item(2), "call_a"),
            (item(3), "call_b"),
            (item(4), "call_c"),
            (item(5), "call_d"),
        ];

        for count in 1..=all_calls.len() {
            for completion_order in generated_permutations(count) {
                let turn_id = turn(1);
                let mut state = scripted_state(session(1), (1_u8..=6).map(entry));
                start(&mut state, turn_id);
                state
                    .apply_assistant_message(
                        assistant_with_calls(&all_calls[..count]),
                        turn_id,
                        timestamp(),
                    )
                    .unwrap();

                for call_index in completion_order {
                    let (item_id, call_id) = all_calls[call_index];
                    state
                        .apply_tool_message(
                            completed_tool(item_id, call_id, call_id),
                            turn_id,
                            timestamp(),
                        )
                        .unwrap();
                }

                let views = state.capture_conversation_views().unwrap();
                let observed = views.conversation().messages()[2..]
                    .iter()
                    .map(|message| match message.as_ref() {
                        ModelMessageRef::Tool { tool_call_id, .. } => tool_call_id.as_str(),
                        _ => panic!("completed exchange must project only ordered tool results"),
                    })
                    .collect::<Vec<_>>();
                let expected = all_calls[..count]
                    .iter()
                    .map(|(_, call_id)| *call_id)
                    .collect::<Vec<_>>();
                assert_eq!(observed, expected);
                assert_eq!(state.revision, ConversationRevision(3));
            }
        }
    }

    fn deterministic_completion_order(count: usize, seed: u64) -> Vec<usize> {
        let mut order = (0..count).collect::<Vec<_>>();
        let mut entropy = seed ^ (count as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for index in (1..count).rev() {
            entropy = entropy
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            order.swap(index, (entropy % (index as u64 + 1)) as usize);
        }
        order
    }

    #[test]
    fn generated_tool_completion_orders_through_sixteen_calls_promote_once_and_project_call_order()
    {
        for count in 1..=16 {
            for seed in 0..32_u64 {
                let turn_id = turn(1);
                let call_names = (0..count)
                    .map(|index| format!("generated_call_{index}"))
                    .collect::<Vec<_>>();
                let calls = call_names
                    .iter()
                    .enumerate()
                    .map(|(index, call)| (item((index + 2) as u8), call.as_str()))
                    .collect::<Vec<_>>();
                let mut state = scripted_state(
                    session(1),
                    (1..=(count + 2)).map(|number| entry(number as u8)),
                );
                start(&mut state, turn_id);
                state
                    .apply_assistant_message(assistant_with_calls(&calls), turn_id, timestamp())
                    .unwrap();

                let mut previous_revision = state.revision;
                for (completion_index, call_index) in deterministic_completion_order(count, seed)
                    .into_iter()
                    .enumerate()
                {
                    let (item_id, call_id) = calls[call_index];
                    let applied = state
                        .apply_tool_message(
                            completed_tool(item_id, call_id, call_id),
                            turn_id,
                            timestamp(),
                        )
                        .unwrap();
                    let expected_delta = u64::from(completion_index + 1 == count);
                    assert_eq!(
                        applied.revision().0 - previous_revision.0,
                        expected_delta,
                        "count {count}, seed {seed}, completion {completion_index}"
                    );
                    previous_revision = applied.revision();
                }

                let views = state.capture_conversation_views().unwrap();
                let observed = views.conversation().messages()[2..]
                    .iter()
                    .map(|message| match message.as_ref() {
                        ModelMessageRef::Tool { tool_call_id, .. } => tool_call_id.as_str(),
                        _ => panic!("completed exchange must project only ordered tool results"),
                    })
                    .collect::<Vec<_>>();
                let expected = call_names.iter().map(String::as_str).collect::<Vec<_>>();
                assert_eq!(observed, expected, "count {count}, seed {seed}");
                assert_eq!(state.revision, ConversationRevision(3));
            }
        }
    }

    #[test]
    fn complete_tool_exchange_is_prepared_before_allocation_and_retries_with_the_same_sentinel() {
        let turn_id = turn(1);
        let sentinel = entry(3);
        let mut state = scripted_state(session(1), [entry(1), entry(2)]);
        start(&mut state, turn_id);
        let assistant = state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state.script_entry_id_candidates([Err(()), Ok(sentinel)]);
        let snapshot = StateSnapshot::capture(&state);

        let failed = state.apply_tool_message(
            completed_tool(item(2), "call_a", "terminal"),
            turn_id,
            timestamp(),
        );
        assert_eq!(
            live_error(failed).reason(),
            LiveConversationErrorReason::EntryIdAllocation
        );
        assert_eq!(state.scripted_allocation_calls(), 1);
        snapshot.assert_unchanged_except_script(&state);

        let applied = state
            .apply_tool_message(
                completed_tool(item(2), "call_a", "terminal"),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(applied.entry().entry_id(), sentinel);
        assert_exact_fact_arc(&state, &applied);
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        assert_eq!(
            source.units()[1].first_entry_id(),
            &assistant.entry().entry_id()
        );
        assert_eq!(source.units()[1].kind(), CompactionUnitKind::ToolExchange);
    }

    #[test]
    fn complete_revision_delta_matrix_is_explicit() {
        let turn_id = turn(1);
        let request_id = request(1);
        let mut state = scripted_state(session(1), (1..=8).map(entry));
        let input = start(&mut state, turn_id).revision();
        let ordinary_assistant = state
            .apply_assistant_message(assistant_text(item(2), "ordinary"), turn_id, timestamp())
            .unwrap()
            .revision();
        let steer = state
            .apply_user_message(
                user_message(item(3), UserMessageSource::Steer, "steer"),
                turn_id,
                timestamp(),
            )
            .unwrap()
            .revision();
        let tool_call_assistant = state
            .apply_assistant_message(
                assistant_with_calls(&[(item(4), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap()
            .revision();
        let interaction_request = state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(4), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap()
            .revision();
        let interaction_resolution = match state
            .apply_interaction_resolution(
                InteractionResolutionCandidate::owner_cancellation(
                    request_id,
                    owner_cancellation(),
                )
                .unwrap(),
                timestamp(),
            )
            .unwrap()
        {
            InteractionResolutionApplyOutcome::Applied(fact) => fact.revision(),
            InteractionResolutionApplyOutcome::Idempotent { .. } => {
                panic!("first owner cancellation must append a fact")
            }
        };
        let complete_exchange = state
            .apply_tool_message(
                completed_tool(item(4), "call_a", "terminal"),
                turn_id,
                timestamp(),
            )
            .unwrap()
            .revision();
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", None, None).unwrap(),
        )
        .unwrap();
        let compaction = state
            .apply_compaction(
                source,
                NonZeroUsize::new(4).unwrap(),
                replacement,
                turn_id,
                timestamp(),
            )
            .unwrap()
            .revision();

        let revision_delta_matrix = [
            ("Input", input.0),
            ("Assistant", ordinary_assistant.0),
            ("Steer", steer.0),
            ("Tool-call Assistant", tool_call_assistant.0),
            ("Interaction request", interaction_request.0),
            ("Interaction resolution", interaction_resolution.0),
            ("Complete Tool exchange", complete_exchange.0),
            ("Compaction Replace", compaction.0),
        ];
        assert_eq!(
            revision_delta_matrix,
            [
                ("Input", 1),
                ("Assistant", 2),
                ("Steer", 3),
                ("Tool-call Assistant", 4),
                ("Interaction request", 4),
                ("Interaction resolution", 4),
                ("Complete Tool exchange", 5),
                ("Compaction Replace", 6),
            ]
        );
    }

    #[test]
    fn partial_or_abandoned_tool_exchange_stays_hidden_and_blocks_next_model_and_source_replace() {
        let turn_id = turn(1);
        let calls = [
            (item(2), "call_a"),
            (item(3), "call_b"),
            (item(4), "call_c"),
        ];
        let mut state = scripted_state(session(1), (1..=5).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(assistant_with_calls(&calls), turn_id, timestamp())
            .unwrap();
        let partial = state
            .apply_tool_message(completed_tool(item(2), "call_a", "a"), turn_id, timestamp())
            .unwrap();
        assert_eq!(partial.revision().0, 2);
        let abandoned = state
            .apply_tool_message(abandoned_tool(item(3), "call_b"), turn_id, timestamp())
            .unwrap();
        assert_eq!(abandoned.revision().0, 2);

        let views = state.capture_conversation_views().unwrap();
        assert_eq!(views.conversation().messages().len(), 1);
        assert_eq!(views.compaction_source().units().len(), 1);
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        assert_eq!(
            live_error(state.apply_assistant_message(
                assistant_text(item(4), "must not run"),
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::PendingToolExchange
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);

        let replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", None, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_error(state.apply_compaction(
                views.compaction_source().clone(),
                NonZeroUsize::new(1).unwrap(),
                replacement,
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::PendingToolExchange
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);
    }

    #[test]
    fn invalid_turn_and_duplicate_or_mismatched_tool_results_do_not_allocate_or_change_path() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=6).map(entry));
        start(&mut state, turn_id);
        let start_snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        assert_eq!(
            live_error(state.apply_user_message(
                user_message(item(2), UserMessageSource::Steer, "wrong turn"),
                turn(2),
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::InvalidTurn
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        start_snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);

        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a"), (item(3), "call_b")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let applied = state
            .apply_tool_message(
                completed_tool(item(2), "call_a", "result"),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(applied.revision().0, 2);
        let path_before = state.selected_path.clone();
        let full_snapshot = StateSnapshot::capture(&state);
        let revision_before = state.revision;
        let calls_before = state.scripted_allocation_calls();
        for (body, supplied_turn) in [
            (completed_tool(item(2), "call_a", "result"), turn_id),
            (completed_tool(item(2), "call_a", "conflict"), turn_id),
            (completed_tool(item(2), "wrong_call", "mismatch"), turn_id),
            (completed_tool(item(2), "call_a", "cross turn"), turn(2)),
        ] {
            assert!(
                state
                    .apply_tool_message(body, supplied_turn, timestamp())
                    .is_err()
            );
            assert_eq!(state.scripted_allocation_calls(), calls_before);
            assert_eq!(state.revision, revision_before);
            assert_eq!(state.selected_path.len(), path_before.len());
            assert!(
                state
                    .selected_path
                    .iter()
                    .zip(&path_before)
                    .all(|(left, right)| Arc::ptr_eq(left, right))
            );
            full_snapshot.assert_unchanged(&state);
        }
        assert_post_error_probe_uses_original_sentinel(&state);

        let promotion = state
            .apply_tool_message(
                completed_tool(item(3), "call_b", "second"),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(promotion.revision().0, 3);
        let next = state
            .apply_assistant_message(assistant_text(item(4), "next"), turn_id, timestamp())
            .unwrap();
        assert_eq!(next.entry().entry_id(), entry(5));
    }

    #[test]
    fn interaction_resolution_is_first_wins_idempotent_and_allows_a_later_request_on_the_item() {
        let turn_id = turn(1);
        let request_id = request(1);
        let key = resolution_key(1);
        let mut state = scripted_state(session(1), (1..=7).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let request_fact = state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request_id, item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &request_fact);
        assert_eq!(request_fact.revision().0, 2);

        let captured = state.capture_conversation_views().unwrap();
        assert_eq!(captured.pending_interactions().len(), 1);
        assert_eq!(captured.pending_interactions()[0].request_id(), &request_id);
        assert_eq!(captured.pending_interactions()[0].turn_id(), &turn_id);
        assert_eq!(captured.pending_interactions()[0].item_id(), &item(2));
        assert!(matches!(
            captured.pending_interactions()[0].request(),
            InteractionRequestView::ToolApproval(_)
        ));

        let second_pending_snapshot = StateSnapshot::capture(&state);
        let second_pending_calls = state.scripted_allocation_calls();
        assert_eq!(
            live_error(state.apply_interaction_request(
                InteractionRequestCandidate::new(request(2), item(2), approval_request()),
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::InteractionConflict
        );
        assert_eq!(state.scripted_allocation_calls(), second_pending_calls);
        second_pending_snapshot.assert_unchanged(&state);

        let resolved = state
            .apply_interaction_resolution(
                InteractionResolutionCandidate::host(request_id, key.clone(), approval_denial())
                    .unwrap(),
                timestamp(),
            )
            .unwrap();
        let InteractionResolutionApplyOutcome::Applied(resolved) = resolved else {
            panic!("first terminal resolution must apply");
        };
        assert_exact_fact_arc(&state, &resolved);
        assert_eq!(resolved.revision().0, 2);

        let calls_before_idempotent = state.scripted_allocation_calls();
        let path_before = state.selected_path.clone();
        let idempotent_snapshot = StateSnapshot::capture(&state);
        let idempotent = state
            .apply_interaction_resolution(
                InteractionResolutionCandidate::host(request_id, key, approval_denial()).unwrap(),
                timestamp(),
            )
            .unwrap();
        assert!(matches!(
            idempotent,
            InteractionResolutionApplyOutcome::Idempotent { revision } if revision == ConversationRevision(2)
        ));
        assert_eq!(state.scripted_allocation_calls(), calls_before_idempotent);
        assert_eq!(state.selected_path.len(), path_before.len());
        assert!(
            state
                .selected_path
                .iter()
                .zip(&path_before)
                .all(|(left, right)| Arc::ptr_eq(left, right))
        );
        idempotent_snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);

        let sequential = state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request(2), item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &sequential);
        assert_eq!(
            state
                .capture_conversation_views()
                .unwrap()
                .pending_interactions()
                .len(),
            1
        );
    }

    #[test]
    fn interaction_candidates_enforce_origin_family_and_cross_request_host_key_scope_without_ids() {
        let turn_id = turn(1);
        assert!(
            InteractionResolutionCandidate::owner_cancellation(
                request(1),
                InteractionCancelReason::HostCancelled,
            )
            .is_err()
        );
        assert!(
            InteractionResolutionCandidate::host(
                request(1),
                resolution_key(1),
                ResolvedInteraction::cancelled_by_owner(owner_cancellation()).unwrap(),
            )
            .is_err()
        );

        let mut state = scripted_state(session(1), (1..=8).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a"), (item(3), "call_b")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request(1), item(2), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request(2), item(3), approval_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();

        let shared_key = resolution_key(7);
        assert!(matches!(
            state
                .apply_interaction_resolution(
                    InteractionResolutionCandidate::host(
                        request(1),
                        shared_key.clone(),
                        approval_denial(),
                    )
                    .unwrap(),
                    timestamp(),
                )
                .unwrap(),
            InteractionResolutionApplyOutcome::Applied(_)
        ));
        let shared_key_snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        assert_eq!(
            live_error(
                state.apply_interaction_resolution(
                    InteractionResolutionCandidate::host(request(2), shared_key, approval_denial())
                        .unwrap(),
                    timestamp(),
                )
            )
            .reason(),
            LiveConversationErrorReason::InteractionConflict
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        shared_key_snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);

        let family_mismatch_calls = state.scripted_allocation_calls();
        assert!(matches!(
            state
                .apply_interaction_resolution(
                    InteractionResolutionCandidate::owner_cancellation(
                        request(2),
                        owner_cancellation()
                    )
                    .unwrap(),
                    timestamp(),
                )
                .unwrap(),
            // Owner cancellation is valid for every request family and therefore settles the
            // still-pending second request without changing the revision.
            InteractionResolutionApplyOutcome::Applied(_)
        ));
        assert_eq!(state.scripted_allocation_calls(), family_mismatch_calls + 1);

        state
            .apply_interaction_request(
                InteractionRequestCandidate::new(request(3), item(3), question_request()),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let family_mismatch_snapshot = StateSnapshot::capture(&state);
        let family_mismatch_calls = state.scripted_allocation_calls();
        assert_eq!(
            live_error(
                state.apply_interaction_resolution(
                    InteractionResolutionCandidate::host(
                        request(3),
                        resolution_key(8),
                        approval_denial(),
                    )
                    .unwrap(),
                    timestamp(),
                )
            )
            .reason(),
            LiveConversationErrorReason::InteractionConflict
        );
        assert_eq!(state.scripted_allocation_calls(), family_mismatch_calls);
        family_mismatch_snapshot.assert_unchanged(&state);
        assert_post_error_probe_uses_original_sentinel(&state);
    }

    #[test]
    fn scripted_entropy_collision_and_revision_overflow_preserve_the_first_valid_sentinel() {
        let sentinel = entry(9);
        let mut entropy = LiveSessionState::new(session(1), []);
        entropy.script_entry_id_candidates([Err(()), Ok(sentinel)]);
        let entropy_snapshot = StateSnapshot::capture(&entropy);
        assert_eq!(
            live_error(entropy.apply_user_message(
                user_message(item(1), UserMessageSource::Input, "input"),
                turn(1),
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::EntryIdAllocation
        );
        assert_eq!(entropy.scripted_allocation_calls(), 1);
        entropy_snapshot.assert_unchanged_except_script(&entropy);
        let applied = start(&mut entropy, turn(1));
        assert_eq!(applied.entry().entry_id(), sentinel);

        let collision = entry(8);
        let mut exhausted = LiveSessionState::new(session(1), [collision]);
        exhausted.script_entry_id_candidates(
            std::iter::repeat_n(Ok::<_, ()>(collision), MAX_ENTRY_ID_ALLOCATION_ATTEMPTS)
                .chain(std::iter::once(Ok(sentinel))),
        );
        let exhausted_snapshot = StateSnapshot::capture(&exhausted);
        assert_eq!(
            live_error(exhausted.apply_user_message(
                user_message(item(1), UserMessageSource::Input, "input"),
                turn(1),
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::EntryIdAllocation
        );
        assert_eq!(
            exhausted.scripted_allocation_calls(),
            MAX_ENTRY_ID_ALLOCATION_ATTEMPTS
        );
        exhausted_snapshot.assert_unchanged_except_script(&exhausted);
        assert_eq!(start(&mut exhausted, turn(1)).entry().entry_id(), sentinel);

        let mut overflow = scripted_state(session(1), [sentinel]);
        overflow.revision = ConversationRevision(u64::MAX);
        let overflow_snapshot = StateSnapshot::capture(&overflow);
        assert_eq!(
            live_error(overflow.apply_user_message(
                user_message(item(1), UserMessageSource::Input, "input"),
                turn(1),
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(overflow.scripted_allocation_calls(), 0);
        overflow_snapshot.assert_unchanged(&overflow);
        overflow.revision = ConversationRevision::default();
        assert_eq!(start(&mut overflow, turn(1)).entry().entry_id(), sentinel);
    }

    #[test]
    fn invalid_replacement_summaries_fail_before_compaction_reducer_and_preserve_sentinel() {
        let turn_id = turn(1);
        let sentinel = entry(3);
        let mut state = scripted_state(session(1), [entry(1), entry(2), sentinel]);
        start(&mut state, turn_id);
        state
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let too_long: Arc<str> = "x".repeat(MAX_STORED_COMPACTION_SUMMARY_BYTES + 1).into();

        for (case, summary) in [
            ("Empty", Arc::from("")),
            ("Unsafe", Arc::from("unsafe\r\nsummary")),
            ("TextTooLong", too_long),
        ] {
            let error = CompactionReplacement::for_m4_test(
                StoredCompaction::with_unchecked_summary_for_m4_test(summary, Some(entry(2))),
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid compaction replacement",
                "{case}"
            );
            assert_eq!(state.scripted_allocation_calls(), calls_before, "{case}");
            snapshot.assert_unchanged(&state);
            assert_post_error_probe_uses_original_sentinel(&state);

            let mut compaction_probe = state.clone_for_test();
            snapshot.assert_exact_clone(&compaction_probe);
            let source = compaction_probe
                .capture_conversation_views()
                .unwrap()
                .compaction_source()
                .clone();
            let applied = compaction_probe
                .apply_compaction(
                    source,
                    NonZeroUsize::new(1).unwrap(),
                    CompactionReplacement::for_m4_test(
                        StoredCompaction::reconstruct("valid", Some(entry(2)), None).unwrap(),
                    )
                    .unwrap(),
                    turn_id,
                    timestamp(),
                )
                .unwrap();
            assert_exact_fact_arc(&compaction_probe, &applied);
            assert_eq!(applied.entry().entry_id(), sentinel, "{case}");
        }
    }

    #[test]
    fn revision_overflow_rejects_assistant_final_promotion_and_compaction_before_the_sentinel() {
        let turn_id = turn(1);

        let assistant_sentinel = entry(2);
        let mut assistant = scripted_state(session(1), [entry(1), assistant_sentinel]);
        start(&mut assistant, turn_id);
        let assistant_revision = assistant.revision;
        assistant.revision = ConversationRevision(u64::MAX);
        let assistant_snapshot = StateSnapshot::capture(&assistant);
        let assistant_calls = assistant.scripted_allocation_calls();
        let result = assistant.apply_assistant_message(
            assistant_text(item(2), "answer"),
            turn_id,
            timestamp(),
        );
        assert_eq!(
            live_error(result).reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(assistant.scripted_allocation_calls(), assistant_calls);
        assistant_snapshot.assert_unchanged(&assistant);
        assistant.revision = assistant_revision;
        assert_post_error_probe_uses_original_sentinel(&assistant);

        let promotion_sentinel = entry(3);
        let mut promotion = scripted_state(session(2), [entry(1), entry(2), promotion_sentinel]);
        start(&mut promotion, turn_id);
        promotion
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let promotion_revision = promotion.revision;
        promotion.revision = ConversationRevision(u64::MAX);
        let promotion_snapshot = StateSnapshot::capture(&promotion);
        let promotion_calls = promotion.scripted_allocation_calls();
        let result = promotion.apply_tool_message(
            completed_tool(item(2), "call_a", "result"),
            turn_id,
            timestamp(),
        );
        assert_eq!(
            live_error(result).reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(promotion.scripted_allocation_calls(), promotion_calls);
        promotion_snapshot.assert_unchanged(&promotion);
        promotion.revision = promotion_revision;
        assert_post_error_probe_uses_original_sentinel(&promotion);

        let compaction_sentinel = entry(3);
        let mut compaction = scripted_state(session(3), [entry(1), entry(2), compaction_sentinel]);
        start(&mut compaction, turn_id);
        compaction
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let compaction_revision = compaction.revision;
        compaction.revision = ConversationRevision(u64::MAX);
        let overflow_source = compaction
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let compaction_snapshot = StateSnapshot::capture(&compaction);
        let compaction_calls = compaction.scripted_allocation_calls();
        let result = compaction.apply_compaction(
            overflow_source,
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_eq!(
            live_error(result).reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(compaction.scripted_allocation_calls(), compaction_calls);
        compaction_snapshot.assert_unchanged(&compaction);
        compaction.revision = compaction_revision;
        assert_post_error_probe_uses_original_sentinel(&compaction);

        let restored_snapshot = StateSnapshot::capture(&compaction);
        let mut compaction_probe = compaction.clone_for_test();
        restored_snapshot.assert_exact_clone(&compaction_probe);
        let restored_source = compaction_probe
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let applied = compaction_probe
            .apply_compaction(
                restored_source,
                NonZeroUsize::new(1).unwrap(),
                CompactionReplacement::for_m4_test(
                    StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
                )
                .unwrap(),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&compaction_probe, &applied);
        assert_eq!(applied.entry().entry_id(), compaction_sentinel);
    }

    #[test]
    fn compaction_replaces_only_the_fresh_source_suffix_and_binds_a_new_summary_origin() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=4).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
        )
        .unwrap();

        let applied = state
            .apply_compaction(
                source,
                NonZeroUsize::new(1).unwrap(),
                replacement,
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_exact_fact_arc(&state, &applied);
        assert_eq!(applied.entry().entry_id(), entry(3));
        assert_eq!(applied.entry().parent_id(), Some(entry(2)));
        assert_eq!(applied.revision().0, 3);
        let StoredEntryBody::Compaction(stored) = applied.entry().body() else {
            panic!("compaction apply must append its exact stored marker");
        };
        assert_eq!(stored.first_kept_entry_id(), Some(entry(2)));

        let views = state.capture_conversation_views().unwrap();
        assert_eq!(views.conversation().revision().0, 3);
        assert_eq!(views.conversation().messages().len(), 2);
        assert_eq!(views.compaction_source().units().len(), 2);
        assert_eq!(
            views.compaction_source().units()[0].kind(),
            CompactionUnitKind::RollingSummary
        );
        assert_eq!(
            views.compaction_source().units()[0].first_entry_id(),
            &entry(3)
        );
        assert_eq!(
            views.compaction_source().units()[1].kind(),
            CompactionUnitKind::AssistantMessage
        );
        assert_eq!(
            views.compaction_source().units()[1].first_entry_id(),
            &entry(2)
        );

        let mut all_units = scripted_state(session(2), (1..=3).map(entry));
        start(&mut all_units, turn_id);
        all_units
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let all_source = all_units
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let all_replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("all summarized", None, None).unwrap(),
        )
        .unwrap();
        all_units
            .apply_compaction(
                all_source,
                NonZeroUsize::new(2).unwrap(),
                all_replacement,
                turn_id,
                timestamp(),
            )
            .unwrap();
        let all_views = all_units.capture_conversation_views().unwrap();
        assert_eq!(all_views.compaction_source().units().len(), 1);
        assert_eq!(
            all_views.compaction_source().units()[0].first_entry_id(),
            &entry(3)
        );
        assert_eq!(all_views.conversation().messages().len(), 1);
        assert!(NonZeroUsize::new(0).is_none());
    }

    #[test]
    fn compaction_rejects_stale_cross_session_cut_marker_and_source_factory_failures_without_ids() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=8).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let path_before = state.selected_path.clone();
        let revision_before = state.revision;
        let calls_before = state.scripted_allocation_calls();

        let out_of_range = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", None, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_error(state.apply_compaction(
                source.clone(),
                NonZeroUsize::new(3).unwrap(),
                out_of_range,
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::InvalidCompactionCut
        );
        let wrong_marker = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", None, None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_error(state.apply_compaction(
                source.clone(),
                NonZeroUsize::new(1).unwrap(),
                wrong_marker,
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::CompactionMarkerMismatch
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before);
        assert_eq!(state.revision, revision_before);
        assert!(
            state
                .selected_path
                .iter()
                .zip(&path_before)
                .all(|(left, right)| Arc::ptr_eq(left, right))
        );

        let mut other = scripted_state(session(2), [entry(5), entry(6)]);
        start(&mut other, turn_id);
        other
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let cross_session = other
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let cross_replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_error(state.apply_compaction(
                cross_session,
                NonZeroUsize::new(1).unwrap(),
                cross_replacement,
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::StaleCompactionSource
        );

        state
            .apply_assistant_message(assistant_text(item(3), "newer"), turn_id, timestamp())
            .unwrap();
        let stale_replacement = CompactionReplacement::for_m4_test(
            StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_error(state.apply_compaction(
                source,
                NonZeroUsize::new(1).unwrap(),
                stale_replacement,
                turn_id,
                timestamp(),
            ))
            .reason(),
            LiveConversationErrorReason::StaleCompactionSource
        );

        let corrupt_path = state.selected_path.clone();
        state.stable_units.push(state.stable_units[0].clone());
        assert_eq!(
            live_error(state.capture_conversation_views()).reason(),
            LiveConversationErrorReason::InvalidCompactionSource
        );
        assert_eq!(state.scripted_allocation_calls(), calls_before + 1);
        assert!(
            state
                .selected_path
                .iter()
                .zip(&corrupt_path)
                .all(|(left, right)| Arc::ptr_eq(left, right))
        );
    }

    #[test]
    fn compaction_rejections_use_full_snapshots_for_identity_cut_marker_pending_and_fresh_factory()
    {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=8).map(entry));
        start(&mut state, turn_id);
        state
            .apply_assistant_message(assistant_text(item(2), "answer"), turn_id, timestamp())
            .unwrap();
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();

        let cross_session = Arc::new(
            LiveCompactionSourceView::for_live_reducer(
                session(2),
                *source.revision(),
                source.units().to_vec().into(),
            )
            .unwrap(),
        );
        let snapshot = StateSnapshot::capture(&state);
        let calls_before = state.scripted_allocation_calls();
        let result = state.apply_compaction(
            cross_session,
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::StaleCompactionSource,
        );

        let mut reordered_units = source.units().to_vec();
        reordered_units.reverse();
        let reordered = Arc::new(
            LiveCompactionSourceView::for_live_reducer(
                session(1),
                *source.revision(),
                reordered_units.into(),
            )
            .unwrap(),
        );
        let result = state.apply_compaction(
            reordered,
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::StaleCompactionSource,
        );

        let result = state.apply_compaction(
            source.clone(),
            NonZeroUsize::new(3).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", None, None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidCompactionCut,
        );

        let result = state.apply_compaction(
            source.clone(),
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", None, None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::CompactionMarkerMismatch,
        );

        state.stable_units.push(state.stable_units[0].clone());
        let corrupt_snapshot = StateSnapshot::capture(&state);
        let result = state.apply_compaction(
            source.clone(),
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &state,
            &corrupt_snapshot,
            calls_before,
            result,
            LiveConversationErrorReason::InvalidCompactionSource,
        );

        state.stable_units.pop();
        let applied = state
            .apply_compaction(
                source,
                NonZeroUsize::new(1).unwrap(),
                CompactionReplacement::for_m4_test(
                    StoredCompaction::reconstruct("summary", Some(entry(2)), None).unwrap(),
                )
                .unwrap(),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(applied.entry().entry_id(), entry(3));
        assert_exact_fact_arc(&state, &applied);

        let mut pending = scripted_state(session(3), [entry(1), entry(2), entry(3)]);
        start(&mut pending, turn_id);
        pending
            .apply_assistant_message(
                assistant_with_calls(&[(item(2), "call_a")]),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let pending_source = pending
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        let pending_snapshot = StateSnapshot::capture(&pending);
        let pending_calls = pending.scripted_allocation_calls();
        let result = pending.apply_compaction(
            pending_source,
            NonZeroUsize::new(1).unwrap(),
            CompactionReplacement::for_m4_test(
                StoredCompaction::reconstruct("summary", None, None).unwrap(),
            )
            .unwrap(),
            turn_id,
            timestamp(),
        );
        assert_validation_rejection_unchanged(
            &pending,
            &pending_snapshot,
            pending_calls,
            result,
            LiveConversationErrorReason::PendingToolExchange,
        );
    }

    #[test]
    fn compaction_uses_fresh_messages_and_entry_identity_through_repeated_rolling_summaries() {
        let turn_id = turn(1);
        let mut state = scripted_state(session(1), (1..=6).map(entry));
        start(&mut state, turn_id);
        state
            .apply_user_message(
                user_message(item(2), UserMessageSource::Steer, "input"),
                turn_id,
                timestamp(),
            )
            .unwrap();
        let source = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        assert_eq!(source.units()[0].first_entry_id(), &entry(1));
        assert_eq!(source.units()[1].first_entry_id(), &entry(2));

        let caller_with_different_messages = Arc::new(
            LiveCompactionSourceView::for_live_reducer(
                session(1),
                *source.revision(),
                source
                    .units()
                    .iter()
                    .map(|unit| {
                        PreparedLiveCompactionUnit::for_live_reducer(
                            unit.kind(),
                            Arc::from([ModelMessage::unstamped_user_text(Arc::from(
                                "caller-mutated",
                            ))
                            .unwrap()]),
                        )
                        .unwrap()
                        .bind_origin(*unit.first_entry_id())
                    })
                    .collect::<Vec<_>>()
                    .into(),
            )
            .unwrap(),
        );
        let fresh_suffix_identity = std::ptr::from_ref(&source.units()[1].messages()[0]) as usize;
        let first = state
            .apply_compaction(
                caller_with_different_messages,
                NonZeroUsize::new(1).unwrap(),
                CompactionReplacement::for_m4_test(
                    StoredCompaction::reconstruct("first summary", Some(entry(2)), None).unwrap(),
                )
                .unwrap(),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(first.entry().entry_id(), entry(3));
        let StoredEntryBody::Compaction(first_stored) = first.entry().body() else {
            panic!("first replacement must append a compaction body");
        };
        assert_eq!(first_stored.first_kept_entry_id(), Some(entry(2)));
        let after_first = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        assert_eq!(
            after_first.units()[0].kind(),
            CompactionUnitKind::RollingSummary
        );
        assert_eq!(after_first.units()[0].first_entry_id(), &entry(3));
        assert_eq!(after_first.units()[1].first_entry_id(), &entry(2));
        assert_eq!(
            std::ptr::from_ref(&after_first.units()[1].messages()[0]) as usize,
            fresh_suffix_identity
        );
        assert!(matches!(
            after_first.units()[1].messages()[0].as_ref(),
            ModelMessageRef::User { content } if content[0].as_text() == "input"
        ));

        let second = state
            .apply_compaction(
                after_first,
                NonZeroUsize::new(1).unwrap(),
                CompactionReplacement::for_m4_test(
                    StoredCompaction::reconstruct("second summary", Some(entry(2)), None).unwrap(),
                )
                .unwrap(),
                turn_id,
                timestamp(),
            )
            .unwrap();
        assert_eq!(second.entry().entry_id(), entry(4));
        let after_second = state
            .capture_conversation_views()
            .unwrap()
            .compaction_source()
            .clone();
        assert_eq!(after_second.units().len(), 2);
        assert_eq!(
            after_second.units()[0].kind(),
            CompactionUnitKind::RollingSummary
        );
        assert_eq!(after_second.units()[0].first_entry_id(), &entry(4));
        assert_eq!(after_second.units()[1].first_entry_id(), &entry(2));
    }

    #[test]
    fn revision_starts_at_zero_and_advances_checked() {
        let initial = ConversationRevision::default();

        assert_eq!(initial.0, 0);
        assert_eq!(initial.checked_next().unwrap().0, 1);
        assert_eq!(
            format!("{initial:?}"),
            "ConversationRevision(<process-local>)"
        );
    }

    #[test]
    fn revision_overflow_is_a_redacted_live_conversation_error() {
        let error = ConversationRevision(u64::MAX).checked_next().unwrap_err();

        assert_eq!(
            error.reason(),
            LiveConversationErrorReason::RevisionOverflow
        );
        assert_eq!(error.to_string(), "live conversation operation failed");
        assert_eq!(
            format!("{error:?}"),
            "LiveConversationError { reason: RevisionOverflow }"
        );
    }

    #[test]
    fn seeded_ids_are_reserved() {
        let seeded = entry_id("ent_11111111111111111111111111111111");
        let fresh = entry_id("ent_22222222222222222222222222222222");
        let mut generator = EntryIdGenerator::new([seeded]);
        let mut candidates = [Ok::<_, ()>(seeded), Ok(fresh)].into_iter();

        assert_eq!(
            generator.allocate_candidates(|| candidates.next().unwrap()),
            Ok(fresh)
        );
    }

    #[test]
    fn immediate_unique_allocation_reserves_before_returning() {
        let candidate = entry_id("ent_33333333333333333333333333333333");
        let mut generator = EntryIdGenerator::new([]);

        assert_eq!(
            generator.allocate_candidates(|| Ok::<_, ()>(candidate)),
            Ok(candidate)
        );
        assert_eq!(
            generator.allocate_candidates(|| Ok::<_, ()>(candidate)),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
    }

    #[test]
    fn collision_then_unique_candidate_allocates_the_unique_id() {
        let collision = entry_id("ent_44444444444444444444444444444444");
        let unique = entry_id("ent_55555555555555555555555555555555");
        let mut generator = EntryIdGenerator::new([collision]);
        let mut candidates = [Ok::<_, ()>(collision), Ok(unique)].into_iter();

        assert_eq!(
            generator.allocate_candidates(|| candidates.next().unwrap()),
            Ok(unique)
        );
    }

    #[test]
    fn thirty_two_collisions_exhaust_allocation_attempts() {
        let collision = entry_id("ent_66666666666666666666666666666666");
        let mut generator = EntryIdGenerator::new([collision]);
        let mut attempts = 0;

        assert_eq!(
            generator.allocate_candidates(|| {
                attempts += 1;
                Ok::<_, ()>(collision)
            }),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
        assert_eq!(attempts, MAX_ENTRY_ID_ALLOCATION_ATTEMPTS);
    }

    #[test]
    fn entropy_failure_is_redacted_and_does_not_reserve_the_sentinel() {
        let raw_entropy = "raw entropy detail";
        let sentinel = entry_id("ent_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mut generator = EntryIdGenerator::new([]);
        let reserved_before = generator.reserved.clone();

        let error = generator
            .allocate_candidates(|| Err::<EntryId, _>(raw_entropy))
            .unwrap_err();

        assert_eq!(error, EntryIdAllocationError::EntropyUnavailable);
        assert_eq!(error.to_string(), "entry identifier allocation failed");
        assert!(!format!("{error:?} {error}").contains(raw_entropy));
        assert_eq!(generator.reserved, reserved_before);
        assert_eq!(
            generator.allocate_candidates(|| Ok::<_, ()>(sentinel)),
            Ok(sentinel)
        );
    }

    #[test]
    fn allocation_errors_preserve_reservations_and_the_next_sentinel() {
        let collision = entry_id("ent_77777777777777777777777777777777");
        let sentinel = entry_id("ent_88888888888888888888888888888888");
        let mut generator = EntryIdGenerator::new([collision]);
        let reserved_before = generator.reserved.clone();
        let mut candidates =
            std::iter::repeat_n(Ok::<_, ()>(collision), 32).chain(std::iter::once(Ok(sentinel)));

        assert_eq!(
            generator.allocate_candidates(|| candidates.next().unwrap()),
            Err(EntryIdAllocationError::CollisionAttemptsExhausted)
        );
        assert_eq!(generator.reserved, reserved_before);
        assert_eq!(
            generator.allocate_candidates(|| candidates.next().unwrap()),
            Ok(sentinel)
        );
    }

    #[test]
    fn debug_and_errors_never_disclose_ids_or_entropy_details() {
        let id = entry_id("ent_99999999999999999999999999999999");
        let id_text = id.to_string();
        let raw_entropy = "raw entropy detail";
        let mut generator = EntryIdGenerator::new([id]);
        let error = generator
            .allocate_candidates(|| Err::<EntryId, _>(raw_entropy))
            .unwrap_err();

        assert_eq!(
            format!("{generator:?}"),
            "EntryIdGenerator { reserved_count: 1 }"
        );
        for output in [
            format!("{generator:?}"),
            format!("{error:?}"),
            error.to_string(),
        ] {
            assert!(!output.contains(&id_text));
            assert!(!output.contains(raw_entropy));
        }
    }
}
