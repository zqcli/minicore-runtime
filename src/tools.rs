use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::turn_item_interaction::{
    InteractionRequest, InteractionResolution, ResolvedInteraction,
};
use crate::wire::lexical::{
    LexicalError, canonical_json_string_len, normalize_newlines, validate_opaque_ascii,
    validate_safe_text, validate_stable_symbolic_key,
};
use crate::wire::{BoundedJsonObject, BoundedJsonSchema, ItemId, ProtocolLimits};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolNameError {
    #[error("tool name must be 1..=64 bytes")]
    InvalidLength,
    #[error("tool name violates the stable symbolic key grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolName(Box<str>);

impl ToolName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolName {
    type Err = ToolNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_stable_symbolic_key(value, 64, false).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => ToolNameError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ToolNameError::InvalidGrammar
            }
        })?;
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ToolNameError::InvalidGrammar);
        }
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolCallIdError {
    #[error("tool call ID must be 1..=256 bytes")]
    InvalidLength,
    #[error("tool call ID violates the opaque ASCII grammar")]
    InvalidGrammar,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolCallId(Box<str>);

impl ToolCallId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolCallId {
    type Err = ToolCallIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_opaque_ascii(value, 256).map_err(|error| match error {
            LexicalError::Empty | LexicalError::TooLong => ToolCallIdError::InvalidLength,
            LexicalError::InvalidGrammar | LexicalError::UnsafeText => {
                ToolCallIdError::InvalidGrammar
            }
        })?;
        Ok(Self(value.into()))
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[allow(
    dead_code,
    reason = "serial/parallel policy is consumed by ToolService publication"
)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolExecutionMode {
    Parallel,
    Serial,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolDefinition {
    spec: ToolSpec,
    mode: ToolExecutionMode,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolSpec {
    name: ToolName,
    description: Arc<str>,
    input_schema: BoundedJsonSchema,
}

impl ToolDefinition {
    #[cfg(test)]
    pub(crate) fn new(
        name: ToolName,
        description: impl AsRef<str>,
        input_schema: BoundedJsonSchema,
        mode: ToolExecutionMode,
    ) -> Result<Self, ToolValueError> {
        let description = normalize_and_validate_text(
            description.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )?;
        Ok(Self {
            spec: ToolSpec {
                name,
                description: description.into(),
                input_schema,
            },
            mode,
        })
    }

    pub(crate) const fn name(&self) -> &ToolName {
        self.spec.name()
    }

    pub(crate) const fn mode(&self) -> ToolExecutionMode {
        self.mode
    }
}

impl ToolSpec {
    pub(crate) const fn name(&self) -> &ToolName {
        &self.name
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) const fn input_schema(&self) -> &BoundedJsonSchema {
        &self.input_schema
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolCall {
    tool_call_id: ToolCallId,
    name: ToolName,
    arguments: BoundedJsonObject,
    call_index: u32,
}

impl ToolCall {
    pub(crate) const fn new(
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
        call_index: u32,
    ) -> Self {
        Self {
            tool_call_id,
            name,
            arguments,
            call_index,
        }
    }

    pub(crate) const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub(crate) const fn name(&self) -> &ToolName {
        &self.name
    }

    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "arguments are consumed by concrete Tool executors"
        )
    )]
    pub(crate) const fn arguments(&self) -> &BoundedJsonObject {
        &self.arguments
    }

    pub(crate) const fn call_index(&self) -> u32 {
        self.call_index
    }
}

#[derive(Clone)]
pub(crate) struct ToolExecutionRequest {
    item_id: ItemId,
    call: Arc<ToolCall>,
}

impl ToolExecutionRequest {
    pub(crate) fn new(item_id: ItemId, call: ToolCall) -> Self {
        Self {
            item_id,
            call: Arc::new(call),
        }
    }

    pub(crate) const fn item_id(&self) -> ItemId {
        self.item_id
    }

    pub(crate) const fn call(&self) -> &Arc<ToolCall> {
        &self.call
    }

    /// Exact captured identity: the same ItemId and the same `Arc<ToolCall>` (not merely an
    /// equal call).  The start gate binds one exact request capture, so a foreign request that
    /// merely copies the ids fails the exact-binding invariant.
    pub(crate) fn is_exact_capture(&self, other: &Self) -> bool {
        self.item_id == other.item_id && Arc::ptr_eq(&self.call, &other.call)
    }
}

#[allow(
    dead_code,
    reason = "executor results are produced by cfg(test) scripted executors until the M8 executor lands"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ToolExecutionResult {
    Completed {
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    PreExecution {
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}

impl ToolExecutionResult {
    #[cfg(test)]
    pub(crate) fn completed_text(text: impl AsRef<str>) -> Result<Self, ToolValueError> {
        Ok(Self::Completed {
            disposition: ToolResultDisposition::Succeeded,
            content: ToolResultContent::from_text_parts(vec![text.as_ref().to_owned()])?,
        })
    }
}

pub(crate) type ToolExecutionFuture =
    Pin<Box<dyn Future<Output = ToolExecutionResult> + Send + 'static>>;

/// The bound run future for one started execution: it resolves to the identity-bound
/// `ToolExecutionOutcome` (the executor's `ToolExecutionResult` is bound inside).
pub(crate) type ToolExecutionRun =
    Pin<Box<dyn Future<Output = ToolExecutionOutcome> + Send + 'static>>;

/// The trigger side of one started Tool operation's cancellation pair.
///
/// The Session Execution slot creates the pair only after the typed started proof exists and
/// hands the observer to the start factory; the executor can only observe `cancelled` and
/// performs its own bounded cleanup before returning.  `cancel` is idempotent.
#[derive(Clone)]
pub(crate) struct ToolCancellationHandle {
    token: CancellationToken,
}

/// The observation side of one started Tool operation's cancellation pair: the executor can
/// only observe cancellation, never trigger it.
#[derive(Clone)]
pub(crate) struct ToolCancellationObserver {
    #[allow(
        dead_code,
        reason = "read only by the cooperative-cancellation seam; production ToolSets are empty and only the cancellation tests exercise it"
    )]
    token: CancellationToken,
}

impl ToolCancellationHandle {
    pub(crate) fn new() -> (Self, ToolCancellationObserver) {
        let token = CancellationToken::new();
        (
            Self {
                token: token.clone(),
            },
            ToolCancellationObserver { token },
        )
    }

    /// Idempotent: the token stays cancelled after the first call.
    pub(crate) fn cancel(&self) {
        self.token.cancel();
    }
}

impl ToolCancellationObserver {
    /// Awaits the operation's cancellation: returns when the owning slot cancelled the pair.
    /// This is the executor's cooperative-cancellation seam: a running executor awaits it and
    /// performs its own bounded cleanup before returning.
    #[allow(
        dead_code,
        reason = "awaited by concrete executors; production ToolSets are empty and only the cooperative-cancellation tests exercise it"
    )]
    pub(crate) async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

impl fmt::Debug for ToolCancellationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolCancellationHandle { .. }")
    }
}

impl fmt::Debug for ToolCancellationObserver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolCancellationObserver { .. }")
    }
}

/// The move-only start factory for one exact Tool execution.
///
/// The factory is the only way an executor future comes into existence: it receives the
/// cancellation observer and returns the run future.  `ToolSet::run_started_execution`
/// invokes it only after the typed started proof's exact capture revalidates, so a foreign
/// proof never constructs the factory or any future.
pub(crate) struct ToolExecutionStart {
    factory: Box<dyn FnOnce(ToolCancellationObserver) -> ToolExecutionFuture + Send>,
}

impl ToolExecutionStart {
    /// Test-visible constructor for scripted planners; the M8 executor adapter supplies the
    /// production constructor.
    #[cfg(test)]
    pub(crate) fn new(
        factory: impl FnOnce(ToolCancellationObserver) -> ToolExecutionFuture + Send + 'static,
    ) -> Self {
        Self {
            factory: Box::new(factory),
        }
    }

    fn start(self, observer: ToolCancellationObserver) -> ToolExecutionFuture {
        (self.factory)(observer)
    }
}

/// The synchronous pre-start plan one Tool executor produces for one exact request.
///
/// The plan is produced before any start-gate reservation and its start factory is never
/// invoked by the plan producer: an `Execute` plan holds its factory uninvoked, an
/// `Interaction` plan is presented to the host before the start gate is ever involved, and a
/// `PreExecution` plan carries a frozen preflight result that never reaches the start gate.
/// The start gate decides whether the factory may construct a future; this slice covers
/// exactly that start seam, not a fuller operation slot.
#[allow(
    dead_code,
    reason = "concrete executors/plans are cfg(test) until the M8 executor lands; production ToolSets are empty"
)]
pub(crate) enum ToolExecutionPlan {
    Execute(ToolExecutionStart),
    Interaction {
        request_id: crate::wire::RequestId,
        request: InteractionRequest,
        /// The allowed-path start factory: invoked only after the host allows and the exact
        /// request's start reservation wins at resume.
        allowed: ToolExecutionStart,
        denied: ToolExecutionResult,
    },
    /// A frozen known-tool preflight result (schema, policy, hook, or sandbox preflight): the
    /// request settles as PreExecution without any start-gate reservation and the executor is
    /// never consulted.  Only a truthful pre-execution shape belongs here; a malformed
    /// `Completed` settles under the same fail-closed binding as any other unstarted result.
    PreExecution(ToolExecutionResult),
}

/// Why a start reservation was refused.
///
/// The closed gate distinguishes the two causes so callers can settle truthfully: a
/// cancellation before start produces a matching PreExecution Cancelled ToolResult, while an
/// exact-binding violation is an invariant that fails closed to Abandoned.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ToolStartError {
    #[error("the tool start gate is closed by a cancellation before start")]
    CancelledBeforeStart,
    #[error("the tool start gate is closed by an invalid or exact-binding invariant")]
    InvalidBinding,
}

/// The owner-local first-wins start seam for one exact Tool execution.
///
/// The gate owns a single reserve entry per exact `ToolExecutionRequest` capture as one
/// lock-free atomic slot and knows nothing about EmergencyControl: the Session Execution
/// concrete gate orders reservation against the emergency signal by running the atomic
/// reservation while the emergency owner mutex is held, so `signal` and `reserve` linearize
/// on one mutex.
#[derive(Clone)]
pub(crate) struct ToolStartGate {
    inner: Arc<ToolStartGateInner>,
}

struct ToolStartGateInner {
    request: ToolExecutionRequest,
    state: AtomicU8,
}

/// The four closed gate states as one lock-free slot.
///
/// Every transition is a compare-and-swap on the single `AtomicU8` (the exact transitions
/// are documented on the gate methods).  The slot is never locked, so the gate has no mutex
/// to poison and holds no sibling lock next to the Emergency owner mutex.  All transitions
/// are read-modify-write operations on the one atomic, so they linearize in a single
/// modification order; the `AcqRel`/`Acquire` orderings make each transition visible to
/// later readers without any stronger fence, and the bound request is immutable beside the
/// slot, so no payload ordering is needed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ToolStartGateState {
    Prepared = 0,
    Reserved = 1,
    Started = 2,
    Cancelled = 3,
}

impl ToolStartGateState {
    fn from_u8(value: u8) -> Self {
        // Discriminants are declared on the enum above.
        match value {
            0 => Self::Prepared,
            1 => Self::Reserved,
            2 => Self::Started,
            3 => Self::Cancelled,
            _ => unreachable!("the gate atomic only ever stores one of the four closed states"),
        }
    }
}

impl ToolStartGate {
    pub(crate) fn new(request: ToolExecutionRequest) -> Self {
        Self {
            inner: Arc::new(ToolStartGateInner {
                request,
                state: AtomicU8::new(ToolStartGateState::Prepared as u8),
            }),
        }
    }

    /// Reserves the single start entry for the exact bound request.
    ///
    /// A foreign request, a duplicate reservation, or a reservation after start is an
    /// exact-binding invariant (`InvalidBinding`); a gate explicitly closed before start
    /// returns `CancelledBeforeStart`.  The reservation is an exact-capture check plus one
    /// lock-free Prepared→Reserved compare-and-swap: it never locks (no poison) and performs
    /// no caller work.
    pub(crate) fn reserve(
        &self,
        request: &ToolExecutionRequest,
    ) -> Result<ToolStartPermit, ToolStartError> {
        if !request.is_exact_capture(&self.inner.request) {
            return Err(ToolStartError::InvalidBinding);
        }
        match self.inner.state.compare_exchange(
            ToolStartGateState::Prepared as u8,
            ToolStartGateState::Reserved as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(ToolStartPermit {
                inner: Arc::clone(&self.inner),
            }),
            Err(actual) => match ToolStartGateState::from_u8(actual) {
                ToolStartGateState::Cancelled => Err(ToolStartError::CancelledBeforeStart),
                ToolStartGateState::Reserved | ToolStartGateState::Started => {
                    Err(ToolStartError::InvalidBinding)
                }
                ToolStartGateState::Prepared => {
                    unreachable!("a failed Prepared→Reserved CAS cannot observe Prepared")
                }
            },
        }
    }

    /// Explicitly closes the gate before start.  First-wins with any reservation as lock-free
    /// compare-and-swaps on the one slot: returns whether this call closed a still-reservable
    /// gate (Prepared or Reserved; an outstanding permit then fails to start with
    /// `CancelledBeforeStart` and its drop cannot roll the Cancelled gate back).
    #[allow(
        dead_code,
        reason = "the explicit pre-start close is the M8.3 signal-first seam exercised by Tools unit tests"
    )]
    pub(crate) fn cancel_before_start(&self) -> bool {
        let state = &self.inner.state;
        if state
            .compare_exchange(
                ToolStartGateState::Prepared as u8,
                ToolStartGateState::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return true;
        }
        state
            .compare_exchange(
                ToolStartGateState::Reserved as u8,
                ToolStartGateState::Cancelled as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

impl fmt::Debug for ToolStartGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolStartGate { .. }")
    }
}

/// The move-only, single-use start permit for one exact reserved Tool execution.
///
/// `start` consumes the permit and transitions the gate to Started, producing the typed
/// `ToolStartedExecution` proof; dropping an unused permit rolls the reservation back to
/// Prepared.  The permit never outlives its gate.
pub(crate) struct ToolStartPermit {
    inner: Arc<ToolStartGateInner>,
}

impl ToolStartPermit {
    /// Consumes the permit: the gate transitions Reserved→Started and only a successful
    /// transition constructs the typed started proof.  The start is one lock-free
    /// compare-and-swap: it never locks (no poison) and performs no caller work.
    pub(crate) fn start(self) -> Result<ToolStartedExecution, ToolStartError> {
        match self.inner.state.compare_exchange(
            ToolStartGateState::Reserved as u8,
            ToolStartGateState::Started as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(ToolStartedExecution {
                inner: Arc::clone(&self.inner),
            }),
            Err(actual) => match ToolStartGateState::from_u8(actual) {
                ToolStartGateState::Cancelled => Err(ToolStartError::CancelledBeforeStart),
                ToolStartGateState::Prepared | ToolStartGateState::Started => {
                    Err(ToolStartError::InvalidBinding)
                }
                ToolStartGateState::Reserved => {
                    unreachable!("a failed Reserved→Started CAS cannot observe Reserved")
                }
            },
        }
    }
}

impl Drop for ToolStartPermit {
    fn drop(&mut self) {
        // An unused permit rolls the reservation back; a Cancelled or Started gate stays
        // closed so a dropped permit can never resurrect a start the signal already won.
        let _ = self.inner.state.compare_exchange(
            ToolStartGateState::Reserved as u8,
            ToolStartGateState::Prepared as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl fmt::Debug for ToolStartPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolStartPermit { .. }")
    }
}

/// The move-only, typed proof that one exact Tool execution started.
///
/// Only a successful Reserved→Started transition constructs it, and it binds the exact
/// `ToolExecutionRequest` capture of the started gate.  `ToolSet::run_started_execution`
/// revalidates that exact capture before it polls the executor future, so a foreign proof can
/// never drive a future to an executed outcome.  The proof is single-use and never outlives
/// its gate.
pub(crate) struct ToolStartedExecution {
    inner: Arc<ToolStartGateInner>,
}

impl ToolStartedExecution {
    /// The exact started request capture, read only by the Tools owner's started-binding
    /// path.
    fn request(&self) -> &ToolExecutionRequest {
        &self.inner.request
    }
}

impl fmt::Debug for ToolStartedExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolStartedExecution { .. }")
    }
}

#[derive(Debug)]
pub(crate) enum ToolExecutionOutcome {
    Completed {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        source: ToolOutcomeSource,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    Abandoned {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        reason: ToolAbandonReason,
    },
}

/// The identity-bound fail-closed outcome for one exact request that never ran: a foreign
/// started proof, a panicking settlement task, or any other started-path invariant.
fn abandoned_runtime_failure(request: &ToolExecutionRequest) -> ToolExecutionOutcome {
    ToolExecutionOutcome::Abandoned {
        item_id: request.item_id(),
        tool_call_id: request.call().tool_call_id().clone(),
        reason: ToolAbandonReason::RuntimeFailure,
    }
}

type ToolExecutor = dyn Fn(ToolExecutionRequest) -> ToolExecutionPlan + Send + Sync;

/// The immutable Tool set captured for one Turn.
#[derive(Clone)]
pub(crate) struct ToolSet {
    inner: Arc<ToolSetInner>,
}

struct ToolSetInner {
    definitions: Arc<[ToolDefinition]>,
    specs: Arc<[ToolSpec]>,
    executor: Option<Arc<ToolExecutor>>,
}

/// The model-safe projection of one exact captured [`ToolSet`].
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "the PromptSet captures this owner-bound empty projection in M6.1"
)]
pub(crate) struct ToolPromptView {
    inner: Arc<ToolSetInner>,
}

#[allow(
    dead_code,
    reason = "the empty captured ToolSet is consumed by the pending TurnExecutionContext"
)]
impl ToolSet {
    pub(crate) fn empty() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(ToolSetInner {
                definitions: Arc::from([]),
                specs: Arc::from([]),
                executor: None,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_executor(
        definitions: Vec<ToolDefinition>,
        executor: impl Fn(ToolExecutionRequest) -> ToolExecutionFuture + Send + Sync + 'static,
    ) -> Arc<Self> {
        let executor: Arc<dyn Fn(ToolExecutionRequest) -> ToolExecutionFuture + Send + Sync> =
            Arc::new(executor);
        Self::with_plans(
            definitions,
            Arc::new(move |request| {
                let executor = Arc::clone(&executor);
                // The ignore-cancellation test wrapper: the start factory never surfaces the
                // observer to the scripted executor.
                ToolExecutionPlan::Execute(ToolExecutionStart::new(move |_observer| {
                    executor(request)
                }))
            }),
        )
    }

    /// Test-only cancellable-executor constructor: the scripted executor receives the
    /// cancellation observer so cooperative-cancellation tests can await it and clean up.
    #[cfg(test)]
    pub(crate) fn with_cancellable_executor(
        definitions: Vec<ToolDefinition>,
        executor: impl Fn(ToolExecutionRequest, ToolCancellationObserver) -> ToolExecutionFuture
        + Send
        + Sync
        + 'static,
    ) -> Arc<Self> {
        let executor: Arc<
            dyn Fn(ToolExecutionRequest, ToolCancellationObserver) -> ToolExecutionFuture
                + Send
                + Sync,
        > = Arc::new(executor);
        Self::with_plans(
            definitions,
            Arc::new(move |request| {
                let executor = Arc::clone(&executor);
                ToolExecutionPlan::Execute(ToolExecutionStart::new(move |observer| {
                    executor(request, observer)
                }))
            }),
        )
    }

    /// Test-only planner constructor for scripted pre-start plans.  The planner returns the
    /// exact plan synchronously: an `Interaction` plan is produced before any start-gate
    /// reservation, so the host interaction is presented first and the gate is only involved
    /// at approval resume.
    #[cfg(test)]
    pub(crate) fn with_interaction_planner(
        definitions: Vec<ToolDefinition>,
        planner: impl Fn(ToolExecutionRequest) -> ToolExecutionPlan + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::with_plans(definitions, Arc::new(planner))
    }

    #[cfg(test)]
    fn with_plans(definitions: Vec<ToolDefinition>, executor: Arc<ToolExecutor>) -> Arc<Self> {
        let specs = definitions
            .iter()
            .map(|definition| definition.spec.clone())
            .collect::<Vec<_>>();
        Arc::new(Self {
            inner: Arc::new(ToolSetInner {
                definitions: definitions.into(),
                specs: specs.into(),
                executor: Some(executor),
            }),
        })
    }

    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.inner.definitions
    }

    /// The synchronous pre-start plan for one exact request, or `None` when the tool is
    /// unknown or this ToolSet carries no executor: the caller then settles the identity-bound
    /// unavailable PreExecution result without ever consulting a start gate.
    pub(crate) fn plan(&self, request: &ToolExecutionRequest) -> Option<ToolExecutionPlan> {
        let executor = self.inner.executor.as_ref()?;
        if !self
            .inner
            .definitions
            .iter()
            .any(|definition| definition.name() == request.call().name())
        {
            return None;
        }
        Some(executor(request.clone()))
    }

    /// The identity-bound unavailable outcome: unknown tool or no executor.  No start-gate
    /// permit is involved.
    pub(crate) fn unavailable_outcome(request: &ToolExecutionRequest) -> ToolExecutionOutcome {
        ToolExecutionOutcome::Completed {
            item_id: request.item_id(),
            tool_call_id: request.call().tool_call_id().clone(),
            source: ToolOutcomeSource::PreExecution,
            disposition: ToolResultDisposition::Failed,
            content: ToolResultContent::tool_unavailable(),
        }
    }

    /// The identity-bound cancelled-before-start outcome for one exact request.  Generated by
    /// the Tools owner when the start reservation loses to a signal (or a stale basis): the
    /// executor is never polled, so the round still records a matching PreExecution Cancelled
    /// ToolResult instead of abandoning the whole exchange.
    pub(crate) fn cancelled_before_start_outcome(
        request: &ToolExecutionRequest,
    ) -> ToolExecutionOutcome {
        ToolExecutionOutcome::Completed {
            item_id: request.item_id(),
            tool_call_id: request.call().tool_call_id().clone(),
            source: ToolOutcomeSource::PreExecution,
            disposition: ToolResultDisposition::Cancelled,
            content: ToolResultContent::cancelled_before_start(),
        }
    }

    /// Constructs the run for one already-started execution, or fails closed before it.
    ///
    /// The typed proof is the only entrance to the started path: it is constructed only by a
    /// successful Reserved→Started transition and binds the exact started request capture.
    /// The exact-capture revalidation runs first: a foreign proof (one whose exact capture is
    /// not `request`) fails closed to identity-bound Abandoned RuntimeFailure and the start
    /// factory is never invoked, so no executor future exists and nothing is polled.
    ///
    /// On success the factory is invoked with the cancellation observer and the returned run
    /// future is bound to the exact request.  The caller drives the run future: the Session
    /// Execution slot isolates planner/factory/executor panics through its consuming drive,
    /// so the started path spawns no detached task of its own.
    pub(crate) fn run_started_execution(
        &self,
        request: &ToolExecutionRequest,
        proof: ToolStartedExecution,
        start: ToolExecutionStart,
        observer: ToolCancellationObserver,
    ) -> Result<ToolExecutionRun, ToolExecutionOutcome> {
        if !request.is_exact_capture(proof.request()) {
            return Err(abandoned_runtime_failure(request));
        }
        let future = start.start(observer);
        let request = request.clone();
        Ok(Box::pin(async move {
            Self::bind_started_result(&request, future.await)
        }))
    }

    /// The pre-start settlement decision for one resolved Interaction.
    ///
    /// `Resume` means the host allowed: the caller must revalidate the exact request's start
    /// gate and only then execute the allowed future.  Deny and owner cancellation never
    /// reserve the gate; they settle the exact denied (or cancelled) PreExecution result
    /// directly.
    pub(crate) fn interaction_settlement(
        resolution: &ResolvedInteraction,
        denied: &ToolExecutionResult,
    ) -> InteractionSettlement {
        match resolution.live() {
            InteractionResolution::ToolApproval(ToolApprovalDecision::Deny) => {
                InteractionSettlement::PreExecution(denied.clone())
            }
            InteractionResolution::Cancelled(_) => {
                InteractionSettlement::PreExecution(Self::cancelled_result(denied.clone()))
            }
            InteractionResolution::ToolApproval(_) | InteractionResolution::UserAnswer(_) => {
                InteractionSettlement::Resume
            }
        }
    }

    /// Binds one exact started executor result to its identity-bound outcome.  Only the
    /// typed started-proof path may call this: the proof revalidated the exact capture, so a
    /// `Completed` result truthfully maps to an Executed outcome.  A `Denied` Completed shape
    /// and a `PreExecution` shape after start are invariants that fail closed to Abandoned
    /// OutcomeUnknown.
    fn bind_started_result(
        request: &ToolExecutionRequest,
        result: ToolExecutionResult,
    ) -> ToolExecutionOutcome {
        let item_id = request.item_id();
        let tool_call_id = request.call().tool_call_id().clone();
        match result {
            ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: _,
            } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            },
            ToolExecutionResult::Completed {
                disposition,
                content,
            } => ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition,
                content,
            },
            ToolExecutionResult::PreExecution { .. } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            },
            ToolExecutionResult::Abandoned { reason } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason,
            },
        }
    }

    /// Binds one exact pre-execution result to its identity-bound outcome without any start
    /// proof: a frozen preflight result, an Interaction deny/owner-cancellation, or the
    /// cancelled-before-start settlement.  The executor was never polled, so a `PreExecution`
    /// shape truthfully maps to a PreExecution outcome; a malformed `Completed` shape
    /// (including Denied) is an invariant that fails closed to Abandoned OutcomeUnknown so a
    /// never-started operation can never be disguised as an executed ToolResult.
    pub(crate) fn bind_preexecution_result(
        request: &ToolExecutionRequest,
        result: ToolExecutionResult,
    ) -> ToolExecutionOutcome {
        let item_id = request.item_id();
        let tool_call_id = request.call().tool_call_id().clone();
        match result {
            ToolExecutionResult::Completed { .. } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            },
            ToolExecutionResult::PreExecution {
                disposition,
                content,
            } => ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition,
                content,
            },
            ToolExecutionResult::Abandoned { reason } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason,
            },
        }
    }

    fn cancelled_result(result: ToolExecutionResult) -> ToolExecutionResult {
        match result {
            ToolExecutionResult::Completed { content, .. }
            | ToolExecutionResult::PreExecution { content, .. } => {
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Cancelled,
                    content,
                }
            }
            ToolExecutionResult::Abandoned { reason } => ToolExecutionResult::Abandoned { reason },
        }
    }

    pub(crate) fn prompt_view(&self) -> ToolPromptView {
        ToolPromptView {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn owns_prompt_view(&self, view: &ToolPromptView) -> bool {
        Arc::ptr_eq(&self.inner, &view.inner)
    }
}

/// The pre-start settlement decision for one resolved Interaction.
pub(crate) enum InteractionSettlement {
    /// The host allowed: resume through the exact request's start gate and only then execute
    /// the allowed future.
    Resume,
    /// The host denied or owner-cancelled: settle as pre-execution without any start-gate
    /// reservation.
    PreExecution(ToolExecutionResult),
}

#[allow(
    dead_code,
    reason = "the PromptSet captures this owner-bound empty projection in M6.1"
)]
impl ToolPromptView {
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.specs.is_empty()
    }

    pub(crate) fn specs(&self) -> &[ToolSpec] {
        &self.inner.specs
    }
}

impl fmt::Debug for ToolSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolSet")
            .field("spec_count", &self.inner.definitions.len())
            .finish()
    }
}

impl fmt::Debug for ToolPromptView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolPromptView")
            .field("spec_count", &self.inner.specs.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolValueError {
    #[error("tool text is empty, unsafe, or exceeds its limit")]
    InvalidText,
    #[error("tool result content part count is outside 1..=32")]
    InvalidResultPartCount,
    #[error("tool result content exceeds its aggregate byte limit")]
    ResultContentTooLarge,
    #[error("tool approval request is invalid")]
    InvalidApproval,
    #[error("user question request or answer is invalid")]
    InvalidQuestion,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolResultText(Arc<str>);

impl ToolResultText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToolResultContentPart {
    Text(ToolResultText),
}

impl ToolResultContentPart {
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text(text) => text.as_str(),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolResultContent {
    parts: Arc<[ToolResultContentPart]>,
}

impl ToolResultContent {
    fn tool_unavailable() -> Self {
        Self {
            parts: Arc::from([ToolResultContentPart::Text(ToolResultText(Arc::from(
                "tool is unavailable",
            )))]),
        }
    }

    /// The bounded generic content for one exact tool that was cancelled before its start
    /// gate reservation: the executor was never polled, so no domain content exists.
    fn cancelled_before_start() -> Self {
        Self {
            parts: Arc::from([ToolResultContentPart::Text(ToolResultText(Arc::from(
                "tool did not start",
            )))]),
        }
    }

    pub fn from_text_parts(parts: Vec<String>) -> Result<Self, ToolValueError> {
        if parts.is_empty() || parts.len() > 32 {
            return Err(ToolValueError::InvalidResultPartCount);
        }
        let mut aggregate = 0_usize;
        let mut validated = Vec::with_capacity(parts.len());
        for part in parts {
            let text = validate_external_text(&part, 65_536, true)?;
            aggregate = aggregate
                .checked_add(text.len())
                .ok_or(ToolValueError::ResultContentTooLarge)?;
            if aggregate > 262_144 {
                return Err(ToolValueError::ResultContentTooLarge);
            }
            validated.push(ToolResultContentPart::Text(ToolResultText(text.into())));
        }
        Ok(Self {
            parts: validated.into(),
        })
    }

    pub fn parts(&self) -> &[ToolResultContentPart] {
        &self.parts
    }
}

impl fmt::Debug for ToolResultContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResultContent")
            .field("parts", &self.parts.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolResultDisposition {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolOutcomeSource {
    PreExecution,
    Executed,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolAbandonReason {
    OutcomeUnknown,
    RuntimeFailure,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolRequirementSummaryView {
    filesystem: Option<Arc<str>>,
    network: Option<Arc<str>>,
    process: Option<Arc<str>>,
}

impl ToolRequirementSummaryView {
    fn new(
        filesystem: Option<String>,
        network: Option<String>,
        process: Option<String>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().text.max_public_summary_bytes as usize;
        Ok(Self {
            filesystem: validate_optional_text(filesystem, maximum)?,
            network: validate_optional_text(network, maximum)?,
            process: validate_optional_text(process, maximum)?,
        })
    }

    pub(crate) fn reconstruct(
        filesystem: Option<String>,
        network: Option<String>,
        process: Option<String>,
    ) -> Result<Self, ToolValueError> {
        Self::new(filesystem, network, process)
    }

    pub fn filesystem(&self) -> Option<&str> {
        self.filesystem.as_deref()
    }

    pub fn network(&self) -> Option<&str> {
        self.network.as_deref()
    }

    pub fn process(&self) -> Option<&str> {
        self.process.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalOptionKindView {
    AsRequested,
    Restricted,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolApprovalOptionView {
    option_index: u32,
    kind: ToolApprovalOptionKindView,
    label: Arc<str>,
    effective_requirements: ToolRequirementSummaryView,
}

impl ToolApprovalOptionView {
    fn new(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
        label: impl AsRef<str>,
        effective_requirements: ToolRequirementSummaryView,
    ) -> Result<Self, ToolValueError> {
        let label = normalize_and_validate_text(
            label.as_ref(),
            ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        Ok(Self {
            option_index,
            kind,
            label: label.into(),
            effective_requirements,
        })
    }

    pub(crate) fn reconstruct(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
        label: impl AsRef<str>,
        effective_requirements: ToolRequirementSummaryView,
    ) -> Result<Self, ToolValueError> {
        Self::new(option_index, kind, label, effective_requirements)
    }

    pub const fn option_index(&self) -> u32 {
        self.option_index
    }

    pub const fn kind(&self) -> ToolApprovalOptionKindView {
        self.kind
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub const fn effective_requirements(&self) -> &ToolRequirementSummaryView {
        &self.effective_requirements
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolApprovalRequestView {
    tool_name: ToolName,
    arguments_summary: Arc<str>,
    reason: Arc<str>,
    requirements: ToolRequirementSummaryView,
    options: Arc<[ToolApprovalOptionView]>,
}

impl ToolApprovalRequestView {
    fn new(
        tool_name: ToolName,
        arguments_summary: impl AsRef<str>,
        reason: impl AsRef<str>,
        requirements: ToolRequirementSummaryView,
        options: Vec<ToolApprovalOptionView>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().interaction.max_tool_approval_options as usize;
        if options.is_empty()
            || options.len() > maximum
            || options
                .iter()
                .enumerate()
                .any(|(index, option)| option.option_index() as usize != index)
        {
            return Err(ToolValueError::InvalidApproval);
        }
        let arguments_summary = normalize_and_validate_text(
            arguments_summary.as_ref(),
            ProtocolLimits::v1_0().text.max_public_summary_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        let reason = normalize_and_validate_text(
            reason.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidApproval)?;
        let request = Self {
            tool_name,
            arguments_summary: arguments_summary.into(),
            reason: reason.into(),
            requirements,
            options: options.into(),
        };
        if tool_approval_encoded_len(&request).ok_or(ToolValueError::InvalidApproval)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_view_bytes as usize
        {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(request)
    }

    pub(crate) fn reconstruct(
        tool_name: ToolName,
        arguments_summary: impl AsRef<str>,
        reason: impl AsRef<str>,
        requirements: ToolRequirementSummaryView,
        options: Vec<ToolApprovalOptionView>,
    ) -> Result<Self, ToolValueError> {
        Self::new(tool_name, arguments_summary, reason, requirements, options)
    }

    pub const fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub fn arguments_summary(&self) -> &str {
        &self.arguments_summary
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub const fn requirements(&self) -> &ToolRequirementSummaryView {
        &self.requirements
    }

    pub fn options(&self) -> &[ToolApprovalOptionView] {
        &self.options
    }

    #[allow(dead_code, reason = "consumed by Conversation replay in M3")]
    pub(crate) fn validate_recorded_resolution(
        &self,
        resolution: ToolApprovalResolution,
    ) -> Result<ToolApprovalResolution, ToolValueError> {
        match resolution.as_ref() {
            ToolApprovalResolutionRef::Denied => Ok(resolution),
            ToolApprovalResolutionRef::Allowed { option_index, kind } => {
                let option = self
                    .options()
                    .iter()
                    .find(|option| option.option_index() == option_index)
                    .ok_or(ToolValueError::InvalidApproval)?;
                if option.kind() != kind {
                    return Err(ToolValueError::InvalidApproval);
                }
                Ok(resolution)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalDecisionInput {
    Allow { option_index: u32 },
    Deny,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ToolApprovalResolution {
    kind: ToolApprovalResolutionKind,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum ToolApprovalResolutionKind {
    Allowed {
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    },
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolApprovalResolutionRef {
    Allowed {
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    },
    Denied,
}

impl ToolApprovalResolution {
    pub const fn as_ref(&self) -> ToolApprovalResolutionRef {
        match self.kind {
            ToolApprovalResolutionKind::Allowed { option_index, kind } => {
                ToolApprovalResolutionRef::Allowed { option_index, kind }
            }
            ToolApprovalResolutionKind::Denied => ToolApprovalResolutionRef::Denied,
        }
    }

    pub(crate) const fn reconstruct_allowed(
        option_index: u32,
        kind: ToolApprovalOptionKindView,
    ) -> Self {
        Self {
            kind: ToolApprovalResolutionKind::Allowed { option_index, kind },
        }
    }

    pub(crate) const fn reconstruct_denied() -> Self {
        Self {
            kind: ToolApprovalResolutionKind::Denied,
        }
    }
}

impl fmt::Debug for ToolApprovalResolution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(formatter)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ToolApprovalDecision {
    #[allow(
        dead_code,
        reason = "constructed by future Tool approval execution in M8"
    )]
    AllowOnce,
    #[allow(
        dead_code,
        reason = "the M4 exact-resolution owner retains denial before the M8 host constructor"
    )]
    Deny,
}

#[derive(Clone, Eq, PartialEq)]
struct ToolApprovalOption {
    view: ToolApprovalOptionView,
    decision: ToolApprovalDecision,
}

impl ToolApprovalOption {
    #[cfg(test)]
    fn new(
        view: ToolApprovalOptionView,
        decision: ToolApprovalDecision,
    ) -> Result<Self, ToolValueError> {
        let compatible = matches!(
            (view.kind(), &decision),
            (
                ToolApprovalOptionKindView::AsRequested,
                ToolApprovalDecision::AllowOnce
            )
        );
        if !compatible {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(Self { view, decision })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ToolApprovalRequest {
    view: ToolApprovalRequestView,
    options: Arc<[ToolApprovalOption]>,
}

impl ToolApprovalRequest {
    #[cfg(test)]
    fn new(
        view: ToolApprovalRequestView,
        options: Vec<ToolApprovalOption>,
    ) -> Result<Self, ToolValueError> {
        if options.len() != view.options().len()
            || options
                .iter()
                .zip(view.options())
                .any(|(option, view)| &option.view != view)
        {
            return Err(ToolValueError::InvalidApproval);
        }
        Ok(Self {
            view,
            options: options.into(),
        })
    }

    pub(crate) const fn view(&self) -> &ToolApprovalRequestView {
        &self.view
    }

    pub(crate) fn resolve(
        &self,
        input: ToolApprovalDecisionInput,
    ) -> Result<(ToolApprovalDecision, ToolApprovalResolution), ToolValueError> {
        match input {
            ToolApprovalDecisionInput::Deny => Ok((
                ToolApprovalDecision::Deny,
                ToolApprovalResolution::reconstruct_denied(),
            )),
            ToolApprovalDecisionInput::Allow { option_index } => {
                let option = self
                    .options
                    .iter()
                    .find(|option| option.view.option_index() == option_index)
                    .ok_or(ToolValueError::InvalidApproval)?;
                Ok((
                    option.decision.clone(),
                    ToolApprovalResolution::reconstruct_allowed(option_index, option.view.kind()),
                ))
            }
        }
    }

    /// Validates that both halves of an approval settlement came from this exact request.
    ///
    /// The safe resolution alone is deliberately insufficient for an allow: the private
    /// decision must be the mapping attached to the exact selected option. This keeps the
    /// mapping private to Tools while allowing the interaction owner to validate an opaque
    /// `ResolvedInteraction` before it projects storage facts.
    pub(crate) fn validate_exact_resolution(
        &self,
        decision: &ToolApprovalDecision,
        resolution: &ToolApprovalResolution,
    ) -> Result<(), ToolValueError> {
        match (decision, resolution.as_ref()) {
            (ToolApprovalDecision::Deny, ToolApprovalResolutionRef::Denied) => Ok(()),
            (
                ToolApprovalDecision::AllowOnce,
                ToolApprovalResolutionRef::Allowed { option_index, kind },
            ) => {
                let option = self
                    .options
                    .iter()
                    .find(|option| option.view.option_index() == option_index)
                    .ok_or(ToolValueError::InvalidApproval)?;
                if option.view.kind() != kind || &option.decision != decision {
                    return Err(ToolValueError::InvalidApproval);
                }
                Ok(())
            }
            _ => Err(ToolValueError::InvalidApproval),
        }
    }
}

#[cfg(test)]
pub(crate) fn live_approval_request_fixture() -> ToolApprovalRequest {
    let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
    let option = ToolApprovalOptionView::new(
        0,
        ToolApprovalOptionKindView::AsRequested,
        "Allow once",
        requirements.clone(),
    )
    .unwrap();
    let view = ToolApprovalRequestView::new(
        "write_file".parse().unwrap(),
        "path: src/lib.rs",
        "write requested",
        requirements,
        vec![option.clone()],
    )
    .unwrap();
    ToolApprovalRequest::new(
        view,
        vec![ToolApprovalOption::new(option, ToolApprovalDecision::AllowOnce).unwrap()],
    )
    .unwrap()
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionChoice {
    option_index: u32,
    label: Arc<str>,
}

impl UserQuestionChoice {
    fn new(option_index: u32, label: impl AsRef<str>) -> Result<Self, ToolValueError> {
        let label = normalize_and_validate_text(
            label.as_ref(),
            ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self {
            option_index,
            label: label.into(),
        })
    }

    pub(crate) fn reconstruct(
        option_index: u32,
        label: impl AsRef<str>,
    ) -> Result<Self, ToolValueError> {
        Self::new(option_index, label)
    }

    pub const fn option_index(&self) -> u32 {
        self.option_index
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum UserQuestionInput {
    Text { multiline: bool },
    SingleChoice { options: Arc<[UserQuestionChoice]> },
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionField {
    question_index: u32,
    prompt: Arc<str>,
    required: bool,
    input: UserQuestionInput,
}

impl UserQuestionField {
    fn new(
        question_index: u32,
        prompt: impl AsRef<str>,
        required: bool,
        input: UserQuestionInput,
    ) -> Result<Self, ToolValueError> {
        if let UserQuestionInput::SingleChoice { options } = &input {
            let maximum = ProtocolLimits::v1_0().interaction.max_choices_per_question as usize;
            if options.is_empty()
                || options.len() > maximum
                || !strictly_increasing(options.iter().map(UserQuestionChoice::option_index))
            {
                return Err(ToolValueError::InvalidQuestion);
            }
        }
        let prompt = normalize_and_validate_text(
            prompt.as_ref(),
            ProtocolLimits::v1_0().text.max_description_bytes as usize,
            false,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self {
            question_index,
            prompt: prompt.into(),
            required,
            input,
        })
    }

    pub(crate) fn reconstruct(
        question_index: u32,
        prompt: impl AsRef<str>,
        required: bool,
        input: UserQuestionInput,
    ) -> Result<Self, ToolValueError> {
        Self::new(question_index, prompt, required, input)
    }

    pub const fn question_index(&self) -> u32 {
        self.question_index
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub const fn required(&self) -> bool {
        self.required
    }

    pub const fn input(&self) -> &UserQuestionInput {
        &self.input
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionRequest {
    title: Option<Arc<str>>,
    questions: Arc<[UserQuestionField]>,
}

impl UserQuestionRequest {
    fn new(
        title: Option<String>,
        questions: Vec<UserQuestionField>,
    ) -> Result<Self, ToolValueError> {
        let maximum = ProtocolLimits::v1_0().interaction.max_interaction_questions as usize;
        if questions.is_empty()
            || questions.len() > maximum
            || !strictly_increasing(questions.iter().map(UserQuestionField::question_index))
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        let request = Self {
            title: validate_optional_text(
                title,
                ProtocolLimits::v1_0().text.max_display_name_bytes as usize,
            )
            .map_err(|_| ToolValueError::InvalidQuestion)?,
            questions: questions.into(),
        };
        if user_question_encoded_len(&request).ok_or(ToolValueError::InvalidQuestion)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_view_bytes as usize
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(request)
    }

    pub(crate) fn reconstruct(
        title: Option<String>,
        questions: Vec<UserQuestionField>,
    ) -> Result<Self, ToolValueError> {
        Self::new(title, questions)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn questions(&self) -> &[UserQuestionField] {
        &self.questions
    }

    pub fn validate_answer(
        &self,
        answer: UserQuestionAnswer,
    ) -> Result<UserQuestionAnswer, ToolValueError> {
        let mut answers = answer.answers().iter().peekable();
        for question in self.questions() {
            if answers
                .peek()
                .is_some_and(|answer| answer.question_index() < question.question_index())
            {
                return Err(ToolValueError::InvalidQuestion);
            }
            let matching = answers
                .peek()
                .filter(|answer| answer.question_index() == question.question_index())
                .copied();
            let Some(matching) = matching else {
                if question.required() {
                    return Err(ToolValueError::InvalidQuestion);
                }
                continue;
            };
            answers.next();
            match (question.input(), matching.value()) {
                (UserQuestionInput::Text { .. }, UserQuestionAnswerValue::Text(text)) => {
                    if question.required() && text.is_empty() {
                        return Err(ToolValueError::InvalidQuestion);
                    }
                }
                (
                    UserQuestionInput::SingleChoice { options },
                    UserQuestionAnswerValue::Choice { option_index },
                ) if options
                    .iter()
                    .any(|option| option.option_index() == *option_index) => {}
                _ => return Err(ToolValueError::InvalidQuestion),
            }
        }
        if answers.next().is_some() {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(answer)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub enum UserQuestionAnswerValue {
    Text(Arc<str>),
    Choice { option_index: u32 },
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionFieldAnswer {
    question_index: u32,
    value: UserQuestionAnswerValue,
}

impl UserQuestionFieldAnswer {
    fn new(question_index: u32, value: UserQuestionAnswerValue) -> Self {
        Self {
            question_index,
            value,
        }
    }

    pub fn text(question_index: u32, text: impl AsRef<str>) -> Result<Self, ToolValueError> {
        let text = normalize_and_validate_text(
            text.as_ref(),
            ProtocolLimits::v1_0().interaction.max_answer_text_bytes as usize,
            true,
        )
        .map_err(|_| ToolValueError::InvalidQuestion)?;
        Ok(Self::new(
            question_index,
            UserQuestionAnswerValue::Text(text.into()),
        ))
    }

    pub const fn choice(question_index: u32, option_index: u32) -> Self {
        Self {
            question_index,
            value: UserQuestionAnswerValue::Choice { option_index },
        }
    }

    pub const fn question_index(&self) -> u32 {
        self.question_index
    }

    pub const fn value(&self) -> &UserQuestionAnswerValue {
        &self.value
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct UserQuestionAnswer {
    answers: Arc<[UserQuestionFieldAnswer]>,
}

impl UserQuestionAnswer {
    pub fn new(mut answers: Vec<UserQuestionFieldAnswer>) -> Result<Self, ToolValueError> {
        if answers.len() > ProtocolLimits::v1_0().interaction.max_interaction_questions as usize {
            return Err(ToolValueError::InvalidQuestion);
        }
        let mut aggregate = 0_usize;
        let mut previous = None;
        for answer in &mut answers {
            let index = answer.question_index();
            if previous.is_some_and(|previous| index <= previous) {
                return Err(ToolValueError::InvalidQuestion);
            }
            previous = Some(index);
            if let UserQuestionAnswerValue::Text(text) = &mut answer.value {
                let normalized = normalize_and_validate_text(
                    text,
                    ProtocolLimits::v1_0().interaction.max_answer_text_bytes as usize,
                    true,
                )
                .map_err(|_| ToolValueError::InvalidQuestion)?;
                aggregate = aggregate
                    .checked_add(normalized.len())
                    .ok_or(ToolValueError::InvalidQuestion)?;
                if aggregate
                    > ProtocolLimits::v1_0()
                        .interaction
                        .max_interaction_answer_bytes as usize
                {
                    return Err(ToolValueError::InvalidQuestion);
                }
                *text = normalized.into();
            }
        }
        let answer = Self {
            answers: answers.into(),
        };
        if user_answer_encoded_len(&answer).ok_or(ToolValueError::InvalidQuestion)?
            > ProtocolLimits::v1_0()
                .interaction
                .max_interaction_answer_bytes as usize
        {
            return Err(ToolValueError::InvalidQuestion);
        }
        Ok(answer)
    }

    pub fn answers(&self) -> &[UserQuestionFieldAnswer] {
        &self.answers
    }
}

// Three fixed-width typed IDs plus the pending-interaction object envelope.
const INTERACTION_VIEW_FIXED_BYTES: usize = 159;

fn tool_approval_encoded_len(request: &ToolApprovalRequestView) -> Option<usize> {
    let mut length = INTERACTION_VIEW_FIXED_BYTES;
    add_len(
        &mut length,
        "{\"type\":\"tool_approval\",\"data\":{\"toolName\":".len(),
    )?;
    add_len(
        &mut length,
        canonical_json_string_len(request.tool_name().as_str())?,
    )?;
    add_len(&mut length, ",\"argumentsSummary\":".len())?;
    add_len(
        &mut length,
        canonical_json_string_len(request.arguments_summary())?,
    )?;
    add_len(&mut length, ",\"reason\":".len())?;
    add_len(&mut length, canonical_json_string_len(request.reason())?)?;
    add_len(&mut length, ",\"requirements\":".len())?;
    add_len(
        &mut length,
        requirement_summary_encoded_len(request.requirements())?,
    )?;
    add_len(&mut length, ",\"options\":[".len())?;
    for (index, option) in request.options().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, tool_approval_option_encoded_len(option)?)?;
    }
    add_len(&mut length, "]}}".len())?;
    Some(length)
}

fn tool_approval_option_encoded_len(option: &ToolApprovalOptionView) -> Option<usize> {
    let mut length = "{\"optionIndex\":".len();
    add_len(&mut length, decimal_u32_len(option.option_index()))?;
    add_len(&mut length, ",\"kind\":".len())?;
    let kind = match option.kind() {
        ToolApprovalOptionKindView::AsRequested => "as_requested",
        ToolApprovalOptionKindView::Restricted => "restricted",
    };
    add_len(&mut length, canonical_json_string_len(kind)?)?;
    add_len(&mut length, ",\"label\":".len())?;
    add_len(&mut length, canonical_json_string_len(option.label())?)?;
    add_len(&mut length, ",\"effectiveRequirements\":".len())?;
    add_len(
        &mut length,
        requirement_summary_encoded_len(option.effective_requirements())?,
    )?;
    add_len(&mut length, 1)?;
    Some(length)
}

fn requirement_summary_encoded_len(summary: &ToolRequirementSummaryView) -> Option<usize> {
    let mut length = "{\"filesystem\":".len();
    add_len(
        &mut length,
        optional_string_encoded_len(summary.filesystem())?,
    )?;
    add_len(&mut length, ",\"network\":".len())?;
    add_len(&mut length, optional_string_encoded_len(summary.network())?)?;
    add_len(&mut length, ",\"process\":".len())?;
    add_len(&mut length, optional_string_encoded_len(summary.process())?)?;
    add_len(&mut length, 1)?;
    Some(length)
}

fn user_question_encoded_len(request: &UserQuestionRequest) -> Option<usize> {
    let mut length = INTERACTION_VIEW_FIXED_BYTES;
    add_len(
        &mut length,
        "{\"type\":\"user_question\",\"data\":{\"title\":".len(),
    )?;
    add_len(&mut length, optional_string_encoded_len(request.title())?)?;
    add_len(&mut length, ",\"questions\":[".len())?;
    for (index, question) in request.questions().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, user_question_field_encoded_len(question)?)?;
    }
    add_len(&mut length, "]}}".len())?;
    Some(length)
}

fn user_question_field_encoded_len(question: &UserQuestionField) -> Option<usize> {
    let mut length = "{\"questionIndex\":".len();
    add_len(&mut length, decimal_u32_len(question.question_index()))?;
    add_len(&mut length, ",\"prompt\":".len())?;
    add_len(&mut length, canonical_json_string_len(question.prompt())?)?;
    add_len(&mut length, ",\"required\":".len())?;
    add_len(&mut length, if question.required() { 4 } else { 5 })?;
    add_len(&mut length, ",\"input\":".len())?;
    match question.input() {
        UserQuestionInput::Text { multiline } => {
            add_len(
                &mut length,
                if *multiline {
                    "{\"type\":\"text\",\"data\":{\"multiline\":true}}".len()
                } else {
                    "{\"type\":\"text\",\"data\":{\"multiline\":false}}".len()
                },
            )?;
        }
        UserQuestionInput::SingleChoice { options } => {
            add_len(
                &mut length,
                "{\"type\":\"single_choice\",\"data\":{\"options\":[".len(),
            )?;
            for (index, option) in options.iter().enumerate() {
                if index != 0 {
                    add_len(&mut length, 1)?;
                }
                add_len(&mut length, "{\"optionIndex\":".len())?;
                add_len(&mut length, decimal_u32_len(option.option_index()))?;
                add_len(&mut length, ",\"label\":".len())?;
                add_len(&mut length, canonical_json_string_len(option.label())?)?;
                add_len(&mut length, 1)?;
            }
            add_len(&mut length, "]}}".len())?;
        }
    }
    add_len(&mut length, 1)?;
    Some(length)
}

fn user_answer_encoded_len(answer: &UserQuestionAnswer) -> Option<usize> {
    let mut length = "{\"answers\":[".len();
    for (index, answer) in answer.answers().iter().enumerate() {
        if index != 0 {
            add_len(&mut length, 1)?;
        }
        add_len(&mut length, "{\"questionIndex\":".len())?;
        add_len(&mut length, decimal_u32_len(answer.question_index()))?;
        add_len(&mut length, ",\"value\":".len())?;
        match answer.value() {
            UserQuestionAnswerValue::Text(text) => {
                add_len(&mut length, "{\"type\":\"text\",\"data\":".len())?;
                add_len(&mut length, canonical_json_string_len(text)?)?;
                add_len(&mut length, 1)?;
            }
            UserQuestionAnswerValue::Choice { option_index } => {
                add_len(
                    &mut length,
                    "{\"type\":\"choice\",\"data\":{\"optionIndex\":".len(),
                )?;
                add_len(&mut length, decimal_u32_len(*option_index))?;
                add_len(&mut length, 2)?;
            }
        }
        add_len(&mut length, 1)?;
    }
    add_len(&mut length, 2)?;
    Some(length)
}

fn optional_string_encoded_len(value: Option<&str>) -> Option<usize> {
    value.map_or(Some(4), canonical_json_string_len)
}

fn decimal_u32_len(value: u32) -> usize {
    if value == 0 {
        1
    } else {
        value.ilog10() as usize + 1
    }
}

fn add_len(total: &mut usize, value: usize) -> Option<()> {
    *total = total.checked_add(value)?;
    Some(())
}

fn normalize_and_validate_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<String, ToolValueError> {
    let value = normalize_newlines(value);
    validate_safe_text(&value, maximum, allow_empty).map_err(|_| ToolValueError::InvalidText)?;
    Ok(value)
}

fn validate_external_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<String, ToolValueError> {
    validate_safe_text(value, maximum, allow_empty).map_err(|_| ToolValueError::InvalidText)?;
    Ok(value.to_owned())
}

fn validate_optional_text(
    value: Option<String>,
    maximum: usize,
) -> Result<Option<Arc<str>>, ToolValueError> {
    value
        .map(|value| normalize_and_validate_text(&value, maximum, false).map(Into::into))
        .transpose()
}

fn strictly_increasing(values: impl IntoIterator<Item = u32>) -> bool {
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|previous| value <= previous) {
            return false;
        }
        previous = Some(value);
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::turn_item_interaction::{InteractionCancelReason, InteractionResolutionInput};

    async fn run_planned_execution(
        set: Arc<ToolSet>,
        request: ToolExecutionRequest,
    ) -> ToolExecutionOutcome {
        let ToolExecutionPlan::Execute(start) = set
            .plan(&request)
            .expect("a known tool produces an execution plan")
        else {
            panic!("unexpected interaction plan");
        };
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (_handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        // The production started path settles inside the Session Execution slot's consuming
        // drive, whose catch_unwind isolates a panicking executor; direct tests reproduce
        // that isolation with one spawn of their own.
        let outcome = tokio::spawn(run).await;
        match outcome {
            Ok(outcome) => outcome,
            Err(_) => abandoned_runtime_failure(&request),
        }
    }

    /// Runs one scripted result through the exact proof path: fresh gate, exact reservation,
    /// typed started proof, then the started binding, isolated by one spawn like the Session
    /// Execution slot's consuming drive.
    async fn run_started_with(
        set: Arc<ToolSet>,
        request: ToolExecutionRequest,
        result: ToolExecutionResult,
    ) -> ToolExecutionOutcome {
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (_handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(
                &request,
                proof,
                ToolExecutionStart::new(move |_observer| Box::pin(async move { result })),
                observer,
            )
            .expect("the exact proof revalidates and the factory constructs the run");
        let outcome = tokio::spawn(run).await;
        match outcome {
            Ok(outcome) => outcome,
            Err(_) => abandoned_runtime_failure(&request),
        }
    }

    #[tokio::test]
    async fn captured_tool_set_plans_only_known_tools_and_binds_exact_results() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_executor = Arc::clone(&observed);
        let set = ToolSet::with_executor(
            vec![
                ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON request",
                    "{}".parse().unwrap(),
                    ToolExecutionMode::Parallel,
                )
                .unwrap(),
            ],
            move |request| {
                let observed = Arc::clone(&observed_by_executor);
                Box::pin(async move {
                    let call = request.call();
                    observed.lock().unwrap().push((
                        call.tool_call_id().as_str().to_owned(),
                        call.name().as_str().to_owned(),
                        call.arguments().canonical_json().to_owned(),
                    ));
                    ToolExecutionResult::completed_text("echoed").unwrap()
                })
            },
        );

        let known = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{\"value\":1}".parse().unwrap(),
                0,
            ),
        );
        let outcome = run_planned_execution(Arc::clone(&set), known).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                ref tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ..
            } if item_id == "itm_00000000000000000000000000000001".parse().unwrap()
                && tool_call_id.as_str() == "call_1"
        ));
        assert_eq!(
            *observed.lock().unwrap(),
            [(
                "call_1".to_owned(),
                "echo".to_owned(),
                "{\"value\":1}".to_owned()
            )]
        );

        let unknown = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000002".parse().unwrap(),
            ToolCall::new(
                "call_2".parse().unwrap(),
                "missing".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(set.plan(&unknown).is_none());
        let unknown_outcome = ToolSet::unavailable_outcome(&unknown);
        assert!(matches!(
            unknown_outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                ref tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Failed,
                ref content,
            } if item_id == "itm_00000000000000000000000000000002".parse().unwrap()
                && tool_call_id.as_str() == "call_2"
                && content.parts()[0].as_text() == "tool is unavailable"
        ));
        assert_eq!(observed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn started_executor_panics_map_to_identity_bound_abandoned_outcomes() {
        for mode in [ToolExecutionMode::Parallel, ToolExecutionMode::Serial] {
            let set = ToolSet::with_executor(
                vec![
                    ToolDefinition::new(
                        "echo".parse().unwrap(),
                        "Echo a bounded JSON request",
                        "{}".parse().unwrap(),
                        mode,
                    )
                    .unwrap(),
                ],
                |_| Box::pin(async { panic!("scripted executor panic") }),
            );
            let item_id = "itm_00000000000000000000000000000009".parse().unwrap();
            let request = ToolExecutionRequest::new(
                item_id,
                ToolCall::new(
                    "call_panic".parse().unwrap(),
                    "echo".parse().unwrap(),
                    "{}".parse().unwrap(),
                    0,
                ),
            );
            let outcome = run_planned_execution(Arc::clone(&set), request).await;
            assert!(matches!(
                outcome,
                ToolExecutionOutcome::Abandoned {
                    item_id: actual_item_id,
                    tool_call_id,
                    reason: ToolAbandonReason::RuntimeFailure,
                } if actual_item_id == item_id && tool_call_id.as_str() == "call_panic"
            ));
        }
    }

    #[tokio::test]
    async fn executed_denied_is_not_emitted_as_an_executed_tool_result() {
        let set = ToolSet::with_executor(
            vec![
                ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON request",
                    "{}".parse().unwrap(),
                    ToolExecutionMode::Serial,
                )
                .unwrap(),
            ],
            |_| {
                Box::pin(async {
                    ToolExecutionResult::Completed {
                        disposition: ToolResultDisposition::Denied,
                        content: ToolResultContent::from_text_parts(vec!["denied".to_owned()])
                            .unwrap(),
                    }
                })
            },
        );
        let item_id = "itm_0000000000000000000000000000000a".parse().unwrap();
        let request = ToolExecutionRequest::new(
            item_id,
            ToolCall::new(
                "call_denied".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let outcome = run_planned_execution(Arc::clone(&set), request).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Abandoned {
                item_id: actual_item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            } if actual_item_id == item_id && tool_call_id.as_str() == "call_denied"
        ));
    }

    #[tokio::test]
    async fn started_binding_fails_closed_on_untruthful_shapes_through_the_typed_proof() {
        // The started binding is only reachable through the typed proof: a fresh gate, an
        // exact reservation, and a successful Reserved→Started transition.
        let set = ToolSet::empty();
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(matches!(
            run_started_with(Arc::clone(&set), request.clone(), ToolExecutionResult::completed_text("ran").unwrap()).await,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ..
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
        ));
        // A Denied Completed shape is never a truthful started result.
        assert!(matches!(
            run_started_with(Arc::clone(&set), request.clone(), ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
            })
            .await,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
        ));
        // A PreExecution shape after start is an invariant: a started operation can never be
        // disguised as pre-execution.
        assert!(matches!(
            run_started_with(Arc::clone(&set), request.clone(), ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
            })
            .await,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::OutcomeUnknown,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
        ));
        // An Abandoned result passes through unchanged.
        assert!(matches!(
            run_started_with(Arc::clone(&set), request.clone(), ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            })
            .await,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
        ));
    }

    #[tokio::test]
    async fn foreign_started_proof_never_invokes_the_start_factory_and_fails_closed_identity_bound()
    {
        let constructed = Arc::new(AtomicBool::new(false));
        let constructed_by_factory = Arc::clone(&constructed);
        let set = ToolSet::empty();
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        // A foreign request carrying the same ids but a different capture: the proof is bound
        // to the started gate's exact capture, so this request is not that capture.
        let foreign = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(!request.is_exact_capture(&foreign));
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (_handle, observer) = ToolCancellationHandle::new();
        // The foreign proof fails closed before the factory is invoked: the factory never
        // runs, so no executor future exists and nothing is polled.
        let outcome = match set.run_started_execution(
            &foreign,
            proof,
            ToolExecutionStart::new(move |_observer| {
                constructed_by_factory.store(true, Ordering::Release);
                Box::pin(async { ToolExecutionResult::completed_text("must not run").unwrap() })
            }),
            observer,
        ) {
            Ok(_) => panic!("a foreign proof must fail closed before the factory is invoked"),
            Err(outcome) => outcome,
        };
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == foreign.item_id()
                && tool_call_id == *foreign.call().tool_call_id()
        ));
        assert!(!constructed.load(Ordering::Acquire));
    }

    #[test]
    fn preexecution_binding_fails_closed_on_malformed_completed_shapes() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let pre = ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
        };
        assert!(matches!(
            ToolSet::bind_preexecution_result(&request, pre),
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Denied,
                ..
            }
        ));
        // A malformed Completed shape (including Denied) without a start proof is an
        // invariant: it fails closed to Abandoned OutcomeUnknown instead of fabricating an
        // Executed outcome.
        for malformed in [
            ToolExecutionResult::completed_text("ran").unwrap(),
            ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
            },
        ] {
            assert!(matches!(
                ToolSet::bind_preexecution_result(&request, malformed),
                ToolExecutionOutcome::Abandoned {
                    reason: ToolAbandonReason::OutcomeUnknown,
                    ..
                }
            ));
        }
        assert!(matches!(
            ToolSet::bind_preexecution_result(
                &request,
                ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                },
            ),
            ToolExecutionOutcome::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
                ..
            }
        ));
    }

    #[test]
    fn pre_execution_plan_settles_without_reserving_the_start_gate_or_polling_an_executor() {
        for (disposition, text) in [
            (ToolResultDisposition::Denied, "policy preflight denied"),
            (ToolResultDisposition::Failed, "schema preflight failed"),
        ] {
            let preflight = ToolExecutionResult::PreExecution {
                disposition,
                content: ToolResultContent::from_text_parts(vec![text.to_owned()]).unwrap(),
            };
            let set = ToolSet::with_interaction_planner(
                vec![
                    ToolDefinition::new(
                        "echo".parse().unwrap(),
                        "Echo a bounded JSON request",
                        "{}".parse().unwrap(),
                        ToolExecutionMode::Serial,
                    )
                    .unwrap(),
                ],
                move |_| ToolExecutionPlan::PreExecution(preflight.clone()),
            );
            let request = ToolExecutionRequest::new(
                "itm_0000000000000000000000000000000b".parse().unwrap(),
                ToolCall::new(
                    "call_preflight".parse().unwrap(),
                    "echo".parse().unwrap(),
                    "{}".parse().unwrap(),
                    0,
                ),
            );
            let Some(ToolExecutionPlan::PreExecution(result)) = set.plan(&request) else {
                panic!("a known tool with a frozen preflight produces a PreExecution plan");
            };
            // The PreExecution plan carries no executor future, so settlement can never poll
            // one; bind it exactly the way settle_one_plan does, under the pre-execution
            // binding.
            let outcome = ToolSet::bind_preexecution_result(&request, result);
            assert!(matches!(
                outcome,
                ToolExecutionOutcome::Completed {
                    item_id,
                    ref tool_call_id,
                    source: ToolOutcomeSource::PreExecution,
                    disposition: actual_disposition,
                    ..
                } if item_id == request.item_id()
                    && tool_call_id == request.call().tool_call_id()
                    && actual_disposition == disposition
            ));
            // No reservation happened: the exact request's gate still accepts its single
            // reservation and start.
            let gate = ToolStartGate::new(request.clone());
            assert!(gate.reserve(&request).unwrap().start().is_ok());
        }
    }

    #[test]
    fn interaction_settlement_denies_and_cancels_without_resume_and_allows_resume() {
        let interaction = InteractionRequest::tool_approval(live_approval_request_fixture());
        let denied_result = ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::from_text_parts(vec!["approval denied".to_owned()])
                .unwrap(),
        };
        let denied = interaction
            .resolve_host(InteractionResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Deny,
            ))
            .unwrap();
        let cancelled =
            ResolvedInteraction::cancelled_by_owner(InteractionCancelReason::TurnCancelled)
                .unwrap();
        let allowed = interaction
            .resolve_host(InteractionResolutionInput::ToolApproval(
                ToolApprovalDecisionInput::Allow { option_index: 0 },
            ))
            .unwrap();

        assert!(matches!(
            ToolSet::interaction_settlement(&denied, &denied_result),
            InteractionSettlement::PreExecution(result)
                if matches!(result, ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Denied,
                    ..
                })
        ));
        assert!(matches!(
            ToolSet::interaction_settlement(&cancelled, &denied_result),
            InteractionSettlement::PreExecution(result)
                if matches!(result, ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Cancelled,
                    ..
                })
        ));
        assert!(matches!(
            ToolSet::interaction_settlement(&allowed, &denied_result),
            InteractionSettlement::Resume
        ));
    }

    #[test]
    fn tool_start_gate_reserves_once_starts_once_and_rejects_foreign_or_duplicate_requests() {
        let item_id = "itm_00000000000000000000000000000001".parse().unwrap();
        let request = ToolExecutionRequest::new(
            item_id,
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let gate = ToolStartGate::new(request.clone());
        assert_eq!(format!("{gate:?}"), "ToolStartGate { .. }");

        let permit = gate.reserve(&request).unwrap();
        assert_eq!(format!("{permit:?}"), "ToolStartPermit { .. }");
        // The single reserve entry cannot be reserved twice.
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::InvalidBinding)
        ));
        let proof = permit.start().unwrap();
        assert_eq!(format!("{proof:?}"), "ToolStartedExecution { .. }");
        // The started gate is closed: a later reservation is a duplicate/after-start invariant.
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::InvalidBinding)
        ));

        // A foreign request carrying the same ids but a different capture fails the invariant.
        let foreign = ToolExecutionRequest::new(
            item_id,
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(!request.is_exact_capture(&foreign));
        assert!(matches!(
            gate.reserve(&foreign),
            Err(ToolStartError::InvalidBinding)
        ));

        let other_item = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000002".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(matches!(
            gate.reserve(&other_item),
            Err(ToolStartError::InvalidBinding)
        ));
    }

    #[test]
    fn unused_tool_start_permit_rolls_back_the_reservation() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000003".parse().unwrap(),
            ToolCall::new(
                "call_rollback".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let gate = ToolStartGate::new(request.clone());
        drop(gate.reserve(&request).unwrap());
        assert!(gate.reserve(&request).unwrap().start().is_ok());
    }

    #[test]
    fn cancelled_before_start_gate_never_invokes_the_factory_and_generates_matching_outcome() {
        let constructed = Arc::new(AtomicBool::new(false));
        let constructed_by_executor = Arc::clone(&constructed);
        let set = ToolSet::with_executor(
            vec![
                ToolDefinition::new(
                    "echo".parse().unwrap(),
                    "Echo a bounded JSON request",
                    "{}".parse().unwrap(),
                    ToolExecutionMode::Parallel,
                )
                .unwrap(),
            ],
            move |_| {
                // The executor closure is the ignore-cancellation wrapper's factory body: it
                // runs only when the start factory is invoked.
                constructed_by_executor.store(true, Ordering::Release);
                Box::pin(
                    async move { ToolExecutionResult::completed_text("must not run").unwrap() },
                )
            },
        );
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000004".parse().unwrap(),
            ToolCall::new(
                "call_signal_first".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(matches!(
            set.plan(&request),
            Some(ToolExecutionPlan::Execute(_))
        ));
        let gate = ToolStartGate::new(request.clone());
        assert!(gate.cancel_before_start());
        assert!(!gate.cancel_before_start());
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::CancelledBeforeStart)
        ));
        let outcome = ToolSet::cancelled_before_start_outcome(&request);
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == "tool did not start"
        ));
        // The plan was created synchronously but its start factory was never invoked: the
        // executor closure never ran and no executor future exists.
        assert!(!constructed.load(Ordering::Acquire));
    }

    #[test]
    fn cancelled_gate_keeps_an_outstanding_permit_closed_after_start_attempt() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000005".parse().unwrap(),
            ToolCall::new(
                "call_cancelled_permit".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let gate = ToolStartGate::new(request.clone());
        let permit = gate.reserve(&request).unwrap();
        assert!(gate.cancel_before_start());
        assert!(matches!(
            permit.start(),
            Err(ToolStartError::CancelledBeforeStart)
        ));
        // The consumed permit cannot roll a Cancelled gate back to Prepared.
        assert!(matches!(
            gate.reserve(&request),
            Err(ToolStartError::CancelledBeforeStart)
        ));
    }

    #[test]
    fn empty_prompt_view_retains_its_exact_parent_without_an_identity_value() {
        let first = ToolSet::empty();
        let second = ToolSet::empty();
        let view = first.prompt_view();
        let clone = view.clone();

        assert!(view.is_empty());
        assert!(clone.is_empty());
        assert!(first.owns_prompt_view(&view));
        assert!(first.owns_prompt_view(&clone));
        assert!(!second.owns_prompt_view(&view));
        assert_eq!(format!("{first:?}"), "ToolSet { spec_count: 0 }");
        assert_eq!(format!("{view:?}"), "ToolPromptView { spec_count: 0 }");
    }

    #[test]
    fn result_content_enforces_part_and_aggregate_boundaries() {
        assert!(ToolResultContent::from_text_parts(vec!["x".repeat(65_536)]).is_ok());
        assert!(ToolResultContent::from_text_parts(vec!["x".repeat(65_537)]).is_err());
        assert!(
            ToolResultContent::from_text_parts((0..32).map(|_| "x".to_owned()).collect()).is_ok()
        );
        assert!(
            ToolResultContent::from_text_parts((0..33).map(|_| "x".to_owned()).collect()).is_err()
        );
        assert!(
            ToolResultContent::from_text_parts((0..4).map(|_| "x".repeat(65_536)).collect())
                .is_ok()
        );
        let mut oversized = (0..4).map(|_| "x".repeat(65_536)).collect::<Vec<_>>();
        oversized.push("x".to_owned());
        assert!(ToolResultContent::from_text_parts(oversized).is_err());
    }

    #[test]
    fn approval_owner_validates_the_private_decision_and_safe_resolution_as_one_exact_pair() {
        let request = live_approval_request_fixture();
        let allowed =
            ToolApprovalResolution::reconstruct_allowed(0, ToolApprovalOptionKindView::AsRequested);
        let denied = ToolApprovalResolution::reconstruct_denied();

        assert!(
            request
                .validate_exact_resolution(&ToolApprovalDecision::AllowOnce, &allowed)
                .is_ok()
        );
        assert!(
            request
                .validate_exact_resolution(&ToolApprovalDecision::Deny, &denied)
                .is_ok()
        );
        for (decision, resolution) in [
            (
                ToolApprovalDecision::AllowOnce,
                ToolApprovalResolution::reconstruct_allowed(
                    1,
                    ToolApprovalOptionKindView::AsRequested,
                ),
            ),
            (
                ToolApprovalDecision::AllowOnce,
                ToolApprovalResolution::reconstruct_allowed(
                    0,
                    ToolApprovalOptionKindView::Restricted,
                ),
            ),
            (ToolApprovalDecision::AllowOnce, denied),
            (ToolApprovalDecision::Deny, allowed),
        ] {
            assert_eq!(
                request.validate_exact_resolution(&decision, &resolution),
                Err(ToolValueError::InvalidApproval)
            );
        }
    }

    #[test]
    fn approval_and_question_indices_and_semantic_validation_are_bounded() {
        let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
        let option = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::AsRequested,
            "Allow once",
            requirements.clone(),
        )
        .unwrap();
        let approval_view = ToolApprovalRequestView::new(
            "write_file".parse().unwrap(),
            "path: src/lib.rs",
            "write requested",
            requirements,
            vec![option.clone()],
        )
        .unwrap();
        let approval = ToolApprovalRequest::new(
            approval_view.clone(),
            vec![ToolApprovalOption::new(option, ToolApprovalDecision::AllowOnce).unwrap()],
        )
        .unwrap();
        let (allowed_decision, allowed_resolution) = approval
            .resolve(ToolApprovalDecisionInput::Allow { option_index: 0 })
            .unwrap();
        assert!(matches!(allowed_decision, ToolApprovalDecision::AllowOnce));
        assert!(matches!(
            allowed_resolution.as_ref(),
            ToolApprovalResolutionRef::Allowed {
                option_index: 0,
                kind: ToolApprovalOptionKindView::AsRequested,
            }
        ));
        assert!(matches!(
            approval.resolve(ToolApprovalDecisionInput::Allow { option_index: 1 }),
            Err(ToolValueError::InvalidApproval)
        ));
        let (_, denied_resolution) = approval.resolve(ToolApprovalDecisionInput::Deny).unwrap();
        assert!(matches!(
            denied_resolution.as_ref(),
            ToolApprovalResolutionRef::Denied
        ));
        assert!(
            approval_view
                .validate_recorded_resolution(ToolApprovalResolution::reconstruct_allowed(
                    0,
                    ToolApprovalOptionKindView::AsRequested,
                ))
                .is_ok()
        );
        assert!(matches!(
            approval_view.validate_recorded_resolution(
                ToolApprovalResolution::reconstruct_allowed(
                    0,
                    ToolApprovalOptionKindView::Restricted,
                )
            ),
            Err(ToolValueError::InvalidApproval)
        ));
        assert!(matches!(
            approval_view.validate_recorded_resolution(
                ToolApprovalResolution::reconstruct_allowed(
                    1,
                    ToolApprovalOptionKindView::AsRequested,
                )
            ),
            Err(ToolValueError::InvalidApproval)
        ));
        assert!(
            approval_view
                .validate_recorded_resolution(ToolApprovalResolution::reconstruct_denied())
                .is_ok()
        );

        let restricted_view = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::Restricted,
            "Restricted",
            ToolRequirementSummaryView::new(None, None, None).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ToolApprovalOption::new(restricted_view, ToolApprovalDecision::AllowOnce),
            Err(ToolValueError::InvalidApproval)
        ));

        let choices = vec![
            UserQuestionChoice::new(2, "A").unwrap(),
            UserQuestionChoice::new(4, "B").unwrap(),
        ];
        let first = UserQuestionField::new(
            1,
            "Where?",
            true,
            UserQuestionInput::SingleChoice {
                options: choices.into(),
            },
        )
        .unwrap();
        let second = UserQuestionField::new(
            3,
            "Why?",
            false,
            UserQuestionInput::Text { multiline: true },
        )
        .unwrap();
        let question = UserQuestionRequest::new(None, vec![first, second]).unwrap();

        let valid = UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(1, 4)]).unwrap();
        assert!(question.validate_answer(valid).is_ok());
        let unknown_question =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(5, 4)]).unwrap();
        assert!(question.validate_answer(unknown_question).is_err());
        let missing_required =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(3, "optional").unwrap()])
                .unwrap();
        assert!(question.validate_answer(missing_required).is_err());
        let wrong_family =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(1, "wrong").unwrap()])
                .unwrap();
        assert!(question.validate_answer(wrong_family).is_err());
        let unknown_choice =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(1, 3)]).unwrap();
        assert!(question.validate_answer(unknown_choice).is_err());

        let required_text = UserQuestionRequest::new(
            None,
            vec![
                UserQuestionField::new(
                    0,
                    "Explain",
                    true,
                    UserQuestionInput::Text { multiline: false },
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let empty_text =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "").unwrap()]).unwrap();
        assert!(required_text.validate_answer(empty_text).is_err());

        let answer = UserQuestionFieldAnswer::text(1, "a\r\nb").unwrap();
        let answer = UserQuestionAnswer::new(vec![answer]).unwrap();
        match answer.answers()[0].value() {
            UserQuestionAnswerValue::Text(text) => assert_eq!(text.as_ref(), "a\nb"),
            UserQuestionAnswerValue::Choice { .. } => panic!("wrong answer family"),
        }
    }

    #[test]
    fn interaction_request_size_gates_match_complete_canonical_views() {
        let maximum = ProtocolLimits::v1_0()
            .interaction
            .max_interaction_view_bytes as usize;

        let approval_base = approval_with_extra_text(0).unwrap();
        let approval_extra = maximum - tool_approval_encoded_len(&approval_base).unwrap();
        let approval = approval_with_extra_text(approval_extra).unwrap();
        assert_eq!(tool_approval_encoded_len(&approval), Some(maximum));
        assert_eq!(approval_view_json_len(&approval), maximum);
        assert!(approval_with_extra_text(approval_extra + 1).is_err());

        let question_base = question_with_extra_text(0).unwrap();
        let question_extra = maximum - user_question_encoded_len(&question_base).unwrap();
        let question = question_with_extra_text(question_extra).unwrap();
        assert_eq!(user_question_encoded_len(&question), Some(maximum));
        assert_eq!(question_view_json_len(&question), maximum);
        assert!(question_with_extra_text(question_extra + 1).is_err());
    }

    #[test]
    fn canonical_interaction_sizes_count_quote_and_backslash_expansion() {
        let escaped = "\"\\";
        assert_eq!(canonical_json_string_len(escaped), Some(6));

        let requirements = ToolRequirementSummaryView::new(
            Some(escaped.to_owned()),
            Some(escaped.to_owned()),
            Some(escaped.to_owned()),
        )
        .unwrap();
        let option = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::AsRequested,
            escaped,
            requirements.clone(),
        )
        .unwrap();
        let approval = ToolApprovalRequestView::new(
            "write_file".parse().unwrap(),
            escaped,
            escaped,
            requirements,
            vec![option],
        )
        .unwrap();
        assert_eq!(
            tool_approval_encoded_len(&approval),
            Some(approval_view_json_len(&approval))
        );

        let question = UserQuestionRequest::new(
            Some(escaped.to_owned()),
            vec![
                UserQuestionField::new(
                    0,
                    escaped,
                    false,
                    UserQuestionInput::SingleChoice {
                        options: vec![UserQuestionChoice::new(0, escaped).unwrap()].into(),
                    },
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            user_question_encoded_len(&question),
            Some(question_view_json_len(&question))
        );

        let plain_answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "xx").unwrap()]).unwrap();
        let answer = UserQuestionAnswer::new(vec![
            UserQuestionFieldAnswer::text(0, escaped).unwrap(),
            UserQuestionFieldAnswer::choice(1, 0),
        ])
        .unwrap();
        assert_eq!(
            user_answer_encoded_len(&answer),
            Some(user_answer_json_len(&answer))
        );
        let escaped_text_only =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, escaped).unwrap()])
                .unwrap();
        assert_eq!(
            user_answer_encoded_len(&escaped_text_only).unwrap(),
            user_answer_encoded_len(&plain_answer).unwrap() + 2
        );
    }

    #[test]
    fn user_answer_size_gate_counts_structure_escapes_and_boundary_plus_one() {
        let maximum = ProtocolLimits::v1_0()
            .interaction
            .max_interaction_answer_bytes as usize;
        let empty = UserQuestionAnswer::new(
            (0..4)
                .map(|index| UserQuestionFieldAnswer::text(index, "").unwrap())
                .collect(),
        )
        .unwrap();
        let text_budget = maximum - user_answer_encoded_len(&empty).unwrap();

        let make_answer = |total_text: usize, replacement: Option<char>| {
            let mut remaining = total_text;
            let replacement = replacement.map(|value| value.to_string());
            let answers = (0..4)
                .map(|index| {
                    let size = remaining.min(16_384);
                    remaining -= size;
                    let mut text = "x".repeat(size);
                    if index == 0 {
                        if let Some(replacement) = replacement.as_deref() {
                            text.replace_range(0..replacement.len(), replacement);
                        }
                    }
                    UserQuestionFieldAnswer::text(index, text).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(remaining, 0);
            UserQuestionAnswer::new(answers)
        };

        let boundary = make_answer(text_budget, None).unwrap();
        assert_eq!(user_answer_encoded_len(&boundary), Some(maximum));
        assert!(make_answer(text_budget + 1, None).is_err());
        assert!(make_answer(text_budget, Some('"')).is_err());
        assert!(make_answer(text_budget, Some('\\')).is_err());
    }

    fn approval_with_extra_text(
        mut extra: usize,
    ) -> Result<ToolApprovalRequestView, ToolValueError> {
        fn requirements(extra: &mut usize) -> ToolRequirementSummaryView {
            let mut text = || {
                let additional = (*extra).min(8_191);
                *extra -= additional;
                Some("x".repeat(1 + additional))
            };
            ToolRequirementSummaryView::new(text(), text(), text()).unwrap()
        }

        let top_requirements = requirements(&mut extra);
        let options = (0..16)
            .map(|index| {
                ToolApprovalOptionView::new(
                    index,
                    ToolApprovalOptionKindView::Restricted,
                    "x",
                    requirements(&mut extra),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(extra, 0);
        ToolApprovalRequestView::new(
            "write_file".parse().unwrap(),
            "x",
            "x",
            top_requirements,
            options,
        )
    }

    fn question_with_extra_text(mut extra: usize) -> Result<UserQuestionRequest, ToolValueError> {
        let questions = (0..32)
            .map(|question_index| {
                let prompt_extra = extra.min(8_191);
                extra -= prompt_extra;
                let options = (0..64)
                    .map(|option_index| {
                        let label_extra = extra.min(255);
                        extra -= label_extra;
                        UserQuestionChoice::new(option_index, "x".repeat(1 + label_extra)).unwrap()
                    })
                    .collect::<Vec<_>>();
                UserQuestionField::new(
                    question_index,
                    "x".repeat(1 + prompt_extra),
                    true,
                    UserQuestionInput::SingleChoice {
                        options: options.into(),
                    },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(extra, 0);
        UserQuestionRequest::new(Some("x".to_owned()), questions)
    }

    fn approval_view_json_len(request: &ToolApprovalRequestView) -> usize {
        let requirements = |value: &ToolRequirementSummaryView| {
            serde_json::json!({
                "filesystem": value.filesystem(),
                "network": value.network(),
                "process": value.process(),
            })
        };
        let options = request
            .options()
            .iter()
            .map(|option| {
                serde_json::json!({
                    "optionIndex": option.option_index(),
                    "kind": match option.kind() {
                        ToolApprovalOptionKindView::AsRequested => "as_requested",
                        ToolApprovalOptionKindView::Restricted => "restricted",
                    },
                    "label": option.label(),
                    "effectiveRequirements": requirements(option.effective_requirements()),
                })
            })
            .collect::<Vec<_>>();
        interaction_view_json_len(serde_json::json!({
            "type": "tool_approval",
            "data": {
                "toolName": request.tool_name().as_str(),
                "argumentsSummary": request.arguments_summary(),
                "reason": request.reason(),
                "requirements": requirements(request.requirements()),
                "options": options,
            }
        }))
    }

    fn question_view_json_len(request: &UserQuestionRequest) -> usize {
        let questions = request
            .questions()
            .iter()
            .map(|question| {
                let input = match question.input() {
                    UserQuestionInput::Text { multiline } => {
                        serde_json::json!({"type": "text", "data": {"multiline": multiline}})
                    }
                    UserQuestionInput::SingleChoice { options } => serde_json::json!({
                        "type": "single_choice",
                        "data": {"options": options.iter().map(|option| serde_json::json!({
                            "optionIndex": option.option_index(),
                            "label": option.label(),
                        })).collect::<Vec<_>>()}
                    }),
                };
                serde_json::json!({
                    "questionIndex": question.question_index(),
                    "prompt": question.prompt(),
                    "required": question.required(),
                    "input": input,
                })
            })
            .collect::<Vec<_>>();
        interaction_view_json_len(serde_json::json!({
            "type": "user_question",
            "data": {"title": request.title(), "questions": questions}
        }))
    }

    fn user_answer_json_len(answer: &UserQuestionAnswer) -> usize {
        let answers = answer
            .answers()
            .iter()
            .map(|answer| {
                let value = match answer.value() {
                    UserQuestionAnswerValue::Text(text) => {
                        serde_json::json!({"type": "text", "data": text.as_ref()})
                    }
                    UserQuestionAnswerValue::Choice { option_index } => serde_json::json!({
                        "type": "choice",
                        "data": {"optionIndex": option_index},
                    }),
                };
                serde_json::json!({
                    "questionIndex": answer.question_index(),
                    "value": value,
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&serde_json::json!({"answers": answers}))
            .unwrap()
            .len()
    }

    fn interaction_view_json_len(request: serde_json::Value) -> usize {
        serde_json::to_vec(&serde_json::json!({
            "requestId": "req_00000000000000000000000000000000",
            "turnId": "trn_00000000000000000000000000000000",
            "itemId": "itm_00000000000000000000000000000000",
            "request": request,
        }))
        .unwrap()
        .len()
    }
}
