use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::runtime_task::RuntimeTaskContext;
use crate::wire::lexical::{
    LexicalError, canonical_json_string_len, normalize_newlines, validate_opaque_ascii,
    validate_safe_text, validate_stable_symbolic_key,
};
use crate::wire::{BoundedJsonObject, BoundedJsonSchema, ItemId, ProtocolLimits};
use crate::workspace::WorkspaceToolContext;

/// The M14 production `ask_user` builtin: one closed, default-off, Runtime-owned Tool.
mod ask_user;

/// The production `read_file` builtin: one closed, default-off, Runtime-owned Tool that
/// reads one UTF-8 text file relative to the Workspace cwd.
mod read_file;

/// The production `list_directory` builtin: one closed, default-off, Runtime-owned Tool
/// that lists the direct entries of one directory relative to the Workspace cwd.
mod list_directory;

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
    reason = "executor results are produced by scripted executors or future OS-backed production adapters; the production ask_user builtin never executes"
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
    /// performs its own bounded cleanup before returning.  The production read_file builtin
    /// awaits it before scheduling its blocking read and keeps its tracked job while it runs.
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
    /// The production constructor binding one exact request to its move-only start factory:
    /// the read_file builtin planner uses it, and scripted planners use it in tests.  Only
    /// the owner's `run_started_execution` may invoke the factory, and only after the typed
    /// started proof's exact capture revalidates.
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

#[cfg(test)]
impl ToolExecutionPlan {
    /// Test-visible constructor for the empty default permission set.
    pub(crate) fn execute(start: ToolExecutionStart) -> Self {
        Self::Execute {
            permissions: ToolPermissionSet::new([]),
            start,
        }
    }
}

/// The move-only, redacted answer binding for one exact UserQuestion plan.
///
/// The planner attaches the FnOnce that turns the typed host answer into a ToolResult; only
/// the owner path (`bind`) may invoke it, and the binding is deliberately not Clone: an
/// answer is consumed exactly once by exactly the one question operation that presented it.
pub(crate) struct UserQuestionAnswerBinding {
    request: ToolExecutionRequest,
    bind: Box<dyn FnOnce(UserQuestionAnswer) -> ToolExecutionResult + Send>,
}

impl UserQuestionAnswerBinding {
    /// Test-visible constructor for scripted planners; the M8 executor adapter supplies the
    /// production constructor.
    #[cfg(test)]
    pub(crate) fn new(
        request: ToolExecutionRequest,
        bind: impl FnOnce(UserQuestionAnswer) -> ToolExecutionResult + Send + 'static,
    ) -> Self {
        Self {
            request,
            bind: Box::new(bind),
        }
    }

    /// The owner-only answer bind: consumes the binding, invokes the producer with the typed
    /// host answer, and binds the resulting ToolResult truthfully for the exact request.
    ///
    /// Only a `PreExecution { disposition: Succeeded, .. }` shape is a truthful answered
    /// question result (the question is answered before any execution, and a succeeded
    /// pre-execution is the only disposition that can honestly represent the host answer); an
    /// explicit `Abandoned` passes its reason through unchanged.  Every malformed answered
    /// shape (`Completed`, or a `PreExecution` that is Failed/Denied/Cancelled) is an
    /// invariant that fails closed to identity-bound Abandoned OutcomeUnknown.
    pub(crate) fn bind(
        self,
        request: &ToolExecutionRequest,
        answer: UserQuestionAnswer,
    ) -> ToolExecutionOutcome {
        let item_id = request.item_id();
        let tool_call_id = request.call().tool_call_id().clone();
        if !self.request.is_exact_capture(request) {
            return ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            };
        }
        match (self.bind)(answer) {
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Succeeded,
                content,
            } => ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                content,
            },
            ToolExecutionResult::Abandoned { reason } => ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason,
            },
            ToolExecutionResult::Completed { .. } | ToolExecutionResult::PreExecution { .. } => {
                ToolExecutionOutcome::Abandoned {
                    item_id,
                    tool_call_id,
                    reason: ToolAbandonReason::OutcomeUnknown,
                }
            }
        }
    }
}

impl fmt::Debug for UserQuestionAnswerBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UserQuestionAnswerBinding { .. }")
    }
}

/// The synchronous pre-start plan one Tool executor produces for one exact request.
///
/// The plan is produced before any start-gate reservation and its start factory is never
/// invoked by the plan producer: an `Execute` plan holds its factory uninvoked, an
/// `Approval`/`UserQuestion` plan is presented to the host before the start gate is ever
/// involved, and a `PreExecution` plan carries a frozen preflight result that never reaches
/// the start gate.  The start gate decides whether the factory may construct a future; this
/// slice covers exactly that start seam, not a fuller operation slot.
#[allow(
    dead_code,
    reason = "most executor plan variants remain exercised by scripted tests; the production ask_user builtin uses UserQuestion and PreExecution only"
)]
pub(crate) enum ToolExecutionPlan {
    Execute {
        /// The plan's final class-level permission set (the admission input).
        permissions: ToolPermissionSet,
        start: ToolExecutionStart,
    },
    /// One Tool approval: the host is presented the exact typed approval request before any
    /// start-gate involvement; the allowed-path start factory is invoked only after the host
    /// allows, the plan permissions revalidate against the captured sandbox, and the exact
    /// request's start reservation wins at resume, while a denied or owner-cancelled
    /// approval settles the exact denied (or cancelled) PreExecution result without ever
    /// reserving the gate.
    Approval {
        /// The plan's final permission ceiling, revalidated against the captured sandbox at
        /// settlement; an `AllowWith` candidate may only narrow it.
        permissions: ToolPermissionSet,
        request: ToolApprovalRequest,
        /// The allowed-path start factory: invoked only after the host allows, the final
        /// permission set revalidates, and the exact request's start reservation wins.
        allowed: ToolExecutionStart,
        denied: ToolExecutionResult,
    },
    /// One UserQuestion: the host is presented the exact typed question and the answer is
    /// consumed by the move-only answer binding.  A question never reserves a start gate and
    /// never constructs any start factory: the binding produces the truthful answered
    /// ToolResult (or an explicit Abandoned reason) directly.
    UserQuestion {
        request: UserQuestionRequest,
        answer: UserQuestionAnswerBinding,
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

/// The frozen, closed set of capability classes one Tool Sandbox can enforce: the M13.1
/// surface, class-level only, closed by construction.
#[allow(dead_code, reason = "M13.1 conformance tests")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ToolCapabilityClass {
    FilesystemRead,
    FilesystemWrite,
    Network,
    Process,
}

/// The private class-level value set: one `u8` bitmask with module-private raw bits.
#[allow(dead_code, reason = "M13.1 conformance tests")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ToolCapabilitySet(u8);

#[allow(dead_code, reason = "the M13.1 admission algebra")]
impl ToolCapabilitySet {
    fn from_classes(classes: impl IntoIterator<Item = ToolCapabilityClass>) -> Self {
        Self(classes.into_iter().fold(0, |b, c| b | (1 << c as u8)))
    }

    const fn contains(&self, class: ToolCapabilityClass) -> bool {
        self.0 & (1 << class as u8) != 0
    }

    const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    const fn is_subset_of(&self, other: &Self) -> bool {
        self.0 & !other.0 == 0
    }

    const fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

/// The final class-level permission set for one Tool execution: the admission input.
#[allow(dead_code, reason = "M13.1 conformance tests")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ToolPermissionSet(ToolCapabilitySet);

#[allow(dead_code, reason = "used by M13.1 tests and the M13 wiring slice")]
impl ToolPermissionSet {
    pub(crate) fn new(classes: impl IntoIterator<Item = ToolCapabilityClass>) -> Self {
        Self(ToolCapabilitySet::from_classes(classes))
    }

    pub(crate) const fn contains(&self, class: ToolCapabilityClass) -> bool {
        self.0.contains(class)
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether every class of `self` is carried by `other`: the ceiling revalidation.
    const fn is_subset_of(&self, other: &Self) -> bool {
        self.0.is_subset_of(&other.0)
    }

    /// `self` minus the classes carried by `other`: the exact missing set of an elevation.
    const fn difference(self, other: Self) -> Self {
        Self(self.0.difference(other.0))
    }

    /// The restricted-option candidate: may equal or narrow the ceiling, never widen it.
    pub(crate) fn restricted_candidate(
        &self,
        candidate: impl IntoIterator<Item = ToolCapabilityClass>,
    ) -> Result<Self, ToolPermissionRestrictionError> {
        let candidate = Self::new(candidate);
        candidate
            .0
            .is_subset_of(&self.0)
            .then_some(candidate)
            .ok_or(ToolPermissionRestrictionError::ElevatesCapabilities)
    }
}

/// The closed typed error for a restricted candidate that would widen its ceiling.
#[allow(dead_code, reason = "M13.1 conformance tests")]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ToolPermissionRestrictionError {
    #[error(
        "a restricted permission candidate must not add capability classes beyond the current ceiling"
    )]
    ElevatesCapabilities,
}

/// The adapter's value contract for one Tool Sandbox: availability plus the enforceable
/// class-level set; an unavailable Sandbox carries no set.
#[allow(dead_code, reason = "filled by the M13.1 fake backend")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolSandboxContract {
    Available(ToolCapabilitySet),
    Unavailable,
}

#[allow(dead_code, reason = "exercised by the M13.1 fake backend")]
impl ToolSandboxContract {
    pub(crate) fn available(classes: impl IntoIterator<Item = ToolCapabilityClass>) -> Self {
        Self::Available(ToolCapabilitySet::from_classes(classes))
    }

    pub(crate) const fn unavailable() -> Self {
        Self::Unavailable
    }

    /// The single admission method: admits only when available and every final class is
    /// enforceable, failing a gap as exactly `required − enforceable`.
    pub(crate) fn admit(
        &self,
        permissions: ToolPermissionSet,
    ) -> Result<ToolSandboxProof, ToolSandboxAdmissionError> {
        match self {
            Self::Unavailable => Err(ToolSandboxAdmissionError::Unavailable),
            Self::Available(enforceable) if permissions.0.is_subset_of(enforceable) => {
                Ok(ToolSandboxProof { permissions })
            }
            Self::Available(enforceable) => {
                let missing = ToolPermissionSet(permissions.0.difference(*enforceable));
                Err(ToolSandboxAdmissionError::CapabilityGap { missing })
            }
        }
    }
}

/// The closed admission failure: an unavailable Sandbox and a capability gap are distinct.
#[allow(dead_code, reason = "M13.1 conformance tests")]
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ToolSandboxAdmissionError {
    #[error("the Tool Sandbox is unavailable")]
    Unavailable,
    #[error("the Tool Sandbox cannot enforce the required capability classes")]
    CapabilityGap { missing: ToolPermissionSet },
}

#[allow(dead_code, reason = "M13.1 conformance tests")]
impl ToolSandboxAdmissionError {
    pub(crate) fn missing(&self) -> Option<ToolPermissionSet> {
        match self {
            Self::Unavailable => None,
            Self::CapabilityGap { missing } => Some(*missing),
        }
    }

    /// The owner-side conversion: any admission failure becomes one fixed, bounded,
    /// non-secret `PreExecution { Denied }` text, never naming the missing classes.
    pub(crate) fn denied_result(&self) -> ToolExecutionResult {
        ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::sandbox_denied(match self {
                Self::Unavailable => TOOL_SANDBOX_UNAVAILABLE_TEXT,
                Self::CapabilityGap { .. } => TOOL_CAPABILITY_GAP_TEXT,
            }),
        }
    }
}

/// The move-only proof of admission, binding exactly the admitted final permission set.
#[allow(dead_code, reason = "produced by the M13.1 admission contract")]
pub(crate) struct ToolSandboxProof {
    permissions: ToolPermissionSet,
}

#[allow(dead_code, reason = "read by the M13 gate wiring slice")]
impl ToolSandboxProof {
    pub(crate) fn permissions(&self) -> ToolPermissionSet {
        self.permissions
    }
}

impl fmt::Debug for ToolSandboxProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolSandboxProof { .. }")
    }
}

impl ToolResultContent {
    #[allow(dead_code, reason = "consumed by the M13 gate wiring slice")]
    fn sandbox_denied(text: &'static str) -> Self {
        Self::from_text_parts(vec![text.to_owned()]).expect("the frozen denial texts are valid")
    }
}

#[allow(dead_code, reason = "consumed by the M13 gate wiring slice")]
const TOOL_CAPABILITY_GAP_TEXT: &str = "tool capabilities cannot be enforced";

#[allow(dead_code, reason = "consumed by the M13 gate wiring slice")]
const TOOL_SANDBOX_UNAVAILABLE_TEXT: &str = "tool sandbox is unavailable";

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

/// The owner-internal per-request planner: one request produces its pre-start plan carrying
/// the plan's final permission set on the Execute/Approval shapes; Session Execution only
/// sees the plan.  The M14 production adapter fills this seam with the production planner.
type ToolPlanner = dyn Fn(ToolExecutionRequest) -> ToolExecutionPlan + Send + Sync;

/// The immutable Tool set captured for one Turn.
#[derive(Clone)]
pub(crate) struct ToolSet {
    inner: Arc<ToolSetInner>,
}

struct ToolSetInner {
    definitions: Arc<[ToolDefinition]>,
    specs: Arc<[ToolSpec]>,
    planner: Option<Arc<ToolPlanner>>,
    /// The captured sandbox contract (empty enforceable set by default).
    sandbox: ToolSandboxContract,
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
                planner: None,
                sandbox: ToolSandboxContract::available([]),
            }),
        })
    }

    /// The production opt-in builtin ToolSet: exactly one immutable `ask_user` Tool with its
    /// closed schema, its planner, and the available empty sandbox contract.  `open` selects
    /// exactly one ToolSet (the empty default or this builtin) and passes it through the
    /// existing residency capture; the default Runtime ToolSet stays empty.
    ///
    /// Focused/module tests keep using this method; production selection and composition use
    /// [`ProductionToolConfig`].
    pub(crate) fn ask_user_builtin() -> Arc<Self> {
        ask_user::build_tool_set()
    }

    /// The production opt-in read_file builtin ToolSet: exactly one immutable `read_file`
    /// Tool with its closed schema, its `FilesystemRead` planner, and the available
    /// `FilesystemRead` sandbox contract, pinned to the exact captured Workspace tool
    /// context and the exact Runtime task context.  `open` selects exactly one ToolSet and
    /// passes it through the existing residency capture; the default Runtime ToolSet stays
    /// empty.
    ///
    /// Focused/module tests keep using this method; production selection and composition use
    /// [`ProductionToolConfig`].
    pub(crate) fn read_file_builtin(
        workspace: WorkspaceToolContext,
        task_context: RuntimeTaskContext,
    ) -> Arc<Self> {
        read_file::build_tool_set(workspace, task_context)
    }

    /// The production opt-in list_directory builtin ToolSet: exactly one immutable
    /// `list_directory` Tool with its closed schema, its `FilesystemRead` planner, and the
    /// available `FilesystemRead` sandbox contract, pinned to the exact captured Workspace
    /// tool context and the exact Runtime task context.  `open` selects exactly one ToolSet
    /// and passes it through the existing residency capture; the default Runtime ToolSet
    /// stays empty.
    ///
    /// Focused/module tests keep using this method; production selection and composition use
    /// [`ProductionToolConfig`].
    pub(crate) fn list_directory_builtin(
        workspace: WorkspaceToolContext,
        task_context: RuntimeTaskContext,
    ) -> Arc<Self> {
        list_directory::build_tool_set(workspace, task_context)
    }

    #[cfg(test)]
    pub(crate) fn with_executor(
        definitions: Vec<ToolDefinition>,
        executor: impl Fn(ToolExecutionRequest) -> ToolExecutionFuture + Send + Sync + 'static,
    ) -> Arc<Self> {
        let executor: Arc<dyn Fn(ToolExecutionRequest) -> ToolExecutionFuture + Send + Sync> =
            Arc::new(executor);
        Self::with_sandbox_plans(
            definitions,
            ToolSandboxContract::available([]),
            Arc::new(move |request| {
                let executor = Arc::clone(&executor);
                // The ignore-cancellation test wrapper: the start factory never surfaces the
                // observer to the scripted executor.
                ToolExecutionPlan::Execute {
                    permissions: ToolPermissionSet::new([]),
                    start: ToolExecutionStart::new(move |_observer| executor(request)),
                }
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
        Self::with_sandbox_plans(
            definitions,
            ToolSandboxContract::available([]),
            Arc::new(move |request| {
                let executor = Arc::clone(&executor);
                ToolExecutionPlan::Execute {
                    permissions: ToolPermissionSet::new([]),
                    start: ToolExecutionStart::new(move |observer| executor(request, observer)),
                }
            }),
        )
    }

    /// Test-only planner constructor for scripted pre-start plans.  The planner returns the
    /// exact plan synchronously: an `Approval`/`UserQuestion` plan is produced before any
    /// start-gate reservation, so the host interaction is presented first and the gate is
    /// only involved at approval resume.
    #[cfg(test)]
    pub(crate) fn with_interaction_planner(
        definitions: Vec<ToolDefinition>,
        planner: impl Fn(ToolExecutionRequest) -> ToolExecutionPlan + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::with_sandbox_plans(
            definitions,
            ToolSandboxContract::available([]),
            Arc::new(planner),
        )
    }

    /// Test-only constructor for the M13 admission tests: injects the captured sandbox
    /// contract and a per-request planner producing the plan (carrying its final set).
    #[cfg(test)]
    pub(crate) fn with_sandbox_contract(
        definitions: Vec<ToolDefinition>,
        sandbox: ToolSandboxContract,
        planner: impl Fn(ToolExecutionRequest) -> ToolExecutionPlan + Send + Sync + 'static,
    ) -> Arc<Self> {
        Self::with_sandbox_plans(definitions, sandbox, Arc::new(planner))
    }

    #[cfg(test)]
    fn with_sandbox_plans(
        definitions: Vec<ToolDefinition>,
        sandbox: ToolSandboxContract,
        planner: Arc<ToolPlanner>,
    ) -> Arc<Self> {
        let specs = definitions
            .iter()
            .map(|definition| definition.spec.clone())
            .collect::<Vec<_>>();
        Arc::new(Self {
            inner: Arc::new(ToolSetInner {
                definitions: definitions.into(),
                specs: specs.into(),
                planner: Some(planner),
                sandbox,
            }),
        })
    }

    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.inner.definitions
    }

    /// The synchronous pre-start plan for one exact request, or `None` when the tool is
    /// unknown or this ToolSet carries no planner: the caller then settles the identity-bound
    /// unavailable PreExecution result without ever consulting a start gate.
    ///
    /// The known-tool planner runs exactly once per call; an `Execute` plan is admitted
    /// against the captured sandbox contract before leaving the Tools owner.  A denied
    /// admission yields a frozen `PreExecution` (the uninvoked start factory is dropped,
    /// no gate, no poll); every other plan shape is returned unchanged.
    pub(crate) fn plan(&self, request: &ToolExecutionRequest) -> Option<ToolExecutionPlan> {
        let planner = self.inner.planner.as_ref()?;
        if !self
            .inner
            .definitions
            .iter()
            .any(|definition| definition.name() == request.call().name())
        {
            return None;
        }
        match planner(request.clone()) {
            // Only Execute plans are admitted here; the bound proof is consumed and the
            // admitted plan keeps its final permission set for the started path.
            ToolExecutionPlan::Execute { permissions, start } => {
                match self.inner.sandbox.admit(permissions) {
                    Ok(_) => Some(ToolExecutionPlan::Execute { permissions, start }),
                    Err(error) => Some(ToolExecutionPlan::PreExecution(error.denied_result())),
                }
            }
            plan => Some(plan),
        }
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
    /// the Tools owner when the start reservation (or the exact unstarted settlement) loses to
    /// a signal (or a stale basis): the executor is never polled, so the round still records a
    /// matching PreExecution Cancelled ToolResult instead of abandoning the whole exchange.
    pub(crate) fn cancelled_before_start_outcome(
        request: &ToolExecutionRequest,
    ) -> ToolExecutionOutcome {
        ToolExecutionOutcome::Completed {
            item_id: request.item_id(),
            tool_call_id: request.call().tool_call_id().clone(),
            source: ToolOutcomeSource::PreExecution,
            disposition: ToolResultDisposition::Cancelled,
            content: ToolResultContent::did_not_start(),
        }
    }

    /// The identity-bound failed-before-start outcome for one exact request: an unstarted
    /// plan that was not meant to run at all (an abandoned exchange sibling) settles a
    /// truthful PreExecution Failed with the same generic bounded content as the cancelled
    /// path.  The executor is never polled, so no domain content exists; the request never
    /// reserves a gate.
    pub(crate) fn failed_before_start_outcome(
        request: &ToolExecutionRequest,
    ) -> ToolExecutionOutcome {
        ToolExecutionOutcome::Completed {
            item_id: request.item_id(),
            tool_call_id: request.call().tool_call_id().clone(),
            source: ToolOutcomeSource::PreExecution,
            disposition: ToolResultDisposition::Failed,
            content: ToolResultContent::did_not_start(),
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
        // The start factory is the one caller-supplied synchronous step on the started path
        // (the executor future itself is caught by the caller's run wrapper): a panicking
        // factory must fail closed to the exact request's identity-bound Abandoned outcome
        // while the consumed Prepared parts/request remain owned by the caller, so the slot
        // still consumes itself into Terminal here and no unrelated Terminal is manufactured.
        let future = match std::panic::catch_unwind(AssertUnwindSafe(|| start.start(observer))) {
            Ok(future) => future,
            Err(_) => return Err(abandoned_runtime_failure(request)),
        };
        let request = request.clone();
        Ok(Box::pin(async move {
            Self::bind_started_result(&request, future.await)
        }))
    }

    /// The approval settlement from the exact request's private decision.  `Deny` settles
    /// the exact denied shape fail-closed through the same pre-execution binding as any
    /// other unstarted result.  `AllowOnce` re-admits the plan's final permission set, and
    /// `AllowWith(candidate)` first revalidates in release code that the candidate is within
    /// that ceiling and then re-admits it; every revalidation failure settles the fixed
    /// generic `PreExecution { Denied }` without ever reserving the gate.
    pub(crate) fn approval_settlement(
        &self,
        decision: &ToolApprovalDecision,
        permissions: ToolPermissionSet,
        denied: &ToolExecutionResult,
    ) -> ApprovalSettlement {
        match decision {
            ToolApprovalDecision::Deny => {
                ApprovalSettlement::PreExecution(Self::denied_settlement_result(denied))
            }
            ToolApprovalDecision::AllowOnce => match self.inner.sandbox.admit(permissions) {
                Ok(_) => ApprovalSettlement::Resume,
                Err(error) => ApprovalSettlement::PreExecution(error.denied_result()),
            },
            ToolApprovalDecision::AllowWith(candidate) => {
                // The candidate may only narrow the plan ceiling: elevation fails closed even when the sandbox could enforce it.
                if !candidate.is_subset_of(&permissions) {
                    return ApprovalSettlement::PreExecution(
                        ToolSandboxAdmissionError::CapabilityGap {
                            missing: candidate.difference(permissions),
                        }
                        .denied_result(),
                    );
                }
                match self.inner.sandbox.admit(*candidate) {
                    Ok(_) => ApprovalSettlement::Resume,
                    Err(error) => ApprovalSettlement::PreExecution(error.denied_result()),
                }
            }
        }
    }

    /// The private fail-closed Deny-shape projection: only a valid `PreExecution { disposition:
    /// Denied }` (preserved exactly) or an explicit `Abandoned` (reason preserved) is
    /// accepted; any other `PreExecution` disposition or any `Completed` shape projects
    /// `Abandoned OutcomeUnknown` so the generic pre-execution binder can never emit a
    /// fabricated outcome from a malformed deny plan.
    fn denied_settlement_result(denied: &ToolExecutionResult) -> ToolExecutionResult {
        match denied {
            result @ ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                ..
            }
            | result @ ToolExecutionResult::Abandoned { .. } => result.clone(),
            ToolExecutionResult::Completed { .. }
            | ToolExecutionResult::PreExecution {
                disposition:
                    ToolResultDisposition::Succeeded
                    | ToolResultDisposition::Failed
                    | ToolResultDisposition::Cancelled,
                ..
            } => ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::OutcomeUnknown,
            },
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

    /// The approval cancellation binding: one exact owner/host-cancelled approval settles its
    /// identity-bound cancelled pre-execution outcome directly, without exposing the raw
    /// cancellation rewrite to Session Execution.  Only a valid `PreExecution { disposition:
    /// Denied }` plan is rewritten to Cancelled (content preserved) under the same
    /// pre-execution binding; an explicit `Abandoned` passes its reason through, and any
    /// other `PreExecution` disposition or any malformed `Completed` shape fails closed to
    /// Abandoned OutcomeUnknown — a cancellation is never fabricated from a malformed plan.
    pub(crate) fn bind_approval_cancellation(
        request: &ToolExecutionRequest,
        denied: ToolExecutionResult,
    ) -> ToolExecutionOutcome {
        Self::bind_preexecution_result(request, Self::cancelled_result(denied))
    }

    /// The private approval cancellation rewrite: one exact denied preflight result with its
    /// disposition rewritten to Cancelled (content preserved).  Only a valid `PreExecution`
    /// Denied shape is rewritten; an explicit `Abandoned` passes its reason through
    /// unchanged, and every other shape — a `PreExecution` with any other disposition or a
    /// malformed `Completed` — projects `Abandoned OutcomeUnknown` so the same pre-execution
    /// binding fails it closed and a cancelled approval can never be disguised as an
    /// executed ToolResult or as a different pre-execution outcome.  Only the owner's
    /// `bind_approval_cancellation` uses it; Session Execution never receives the raw result.
    fn cancelled_result(result: ToolExecutionResult) -> ToolExecutionResult {
        match result {
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content,
            } => ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Cancelled,
                content,
            },
            other @ ToolExecutionResult::Abandoned { .. } => other,
            ToolExecutionResult::Completed { .. }
            | ToolExecutionResult::PreExecution {
                disposition:
                    ToolResultDisposition::Succeeded
                    | ToolResultDisposition::Failed
                    | ToolResultDisposition::Cancelled,
                ..
            } => ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::OutcomeUnknown,
            },
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

/// The frozen production Tool composition config: exactly the three closed Runtime-owned
/// builtins (`ask_user`, `read_file`, `list_directory`), each independently opt-in at
/// [`ProductionToolConfig::new`] and never changed afterwards.
///
/// The config freezes its base exactly once at `new` — the empty default or the single
/// `ask_user` builtin — and `for_workspace` composes at the module planner level.  There is
/// no generic registry, no arbitrary merge API, no dynamic callbacks, and no name-based
/// open-world lookup: the composed set contains exactly the three frozen names in one frozen
/// order (`ask_user` → `read_file` → `list_directory`) with only the enabled members, and
/// routes each valid call to exactly one builtin planner, while invalid/unknown names stay
/// unavailable through the normal ToolSet lookup.
#[derive(Clone)]
pub(crate) struct ProductionToolConfig {
    /// Whether the selection includes the frozen `ask_user` builtin.
    ask_user: bool,
    /// Whether the selection includes the workspace-bound `read_file` builtin.
    read_file: bool,
    /// Whether the selection includes the workspace-bound `list_directory` builtin.
    list_directory: bool,
    /// The frozen base captured exactly once at `new`: the empty default or the single
    /// `ask_user` builtin.  With no workspace-bound tool enabled, every `for_workspace`
    /// call returns this exact same captured `Arc`.
    base: Arc<ToolSet>,
}

impl ProductionToolConfig {
    /// Freezes the production selection and its base exactly once: the empty default when
    /// `ask_user` is off, otherwise the exact `ask_user` builtin ToolSet.
    pub(crate) fn new(ask_user: bool, read_file: bool, list_directory: bool) -> Self {
        let base = if ask_user {
            ask_user::build_tool_set()
        } else {
            ToolSet::empty()
        };
        Self {
            ask_user,
            read_file,
            list_directory,
            base,
        }
    }

    /// Materializes the exact selected ToolSet for one Workspace/task context.
    ///
    /// With no workspace-bound tool (`read_file`, `list_directory`) enabled, every call
    /// returns the same captured base `Arc` (the empty default or the single `ask_user`
    /// builtin).  Otherwise the selection is materialized per admission: one immutable
    /// composed set containing exactly the enabled members in the frozen `ask_user` →
    /// `read_file` → `list_directory` order, bound to the exact Workspace tool context and
    /// Runtime task context, with one frozen three-route planner and the `FilesystemRead`
    /// outer sandbox contract.
    pub(crate) fn for_workspace(
        &self,
        workspace: WorkspaceToolContext,
        task_context: RuntimeTaskContext,
    ) -> Arc<ToolSet> {
        if !self.read_file && !self.list_directory {
            return Arc::clone(&self.base);
        }
        composed_production_tool_set(
            self.ask_user,
            self.read_file,
            self.list_directory,
            workspace,
            task_context,
        )
    }
}

/// The closed Tool resource carrier that distinguishes a test-injected immutable [`ToolSet`]
/// (captured at residency start, unchanged by any later reload) from the frozen production
/// opt-in config (materialized per admission against the exact captured Workspace snapshot).
/// There is no generic trait/callback/factory: the two closed variants are the whole surface,
/// and one admission consumes the carrier to materialize exactly once.
#[derive(Clone)]
pub(crate) enum TurnToolResources {
    /// A test-injected immutable ToolSet captured at residency start.
    Captured(Arc<ToolSet>),
    /// The frozen production opt-in config; the per-admission materialization is the only
    /// Tool installation and the shared-resource reload preserves it unchanged.
    Production(ProductionToolConfig),
}

impl TurnToolResources {
    /// Materializes the exact ToolSet for one admission: a `Captured` set returns its Arc
    /// unchanged, while a `Production` config installs its set against the exact captured
    /// Workspace tool context.  Consumes `self` so one admission materializes exactly once.
    pub(crate) fn materialize(
        self,
        workspace: WorkspaceToolContext,
        task_context: RuntimeTaskContext,
    ) -> Arc<ToolSet> {
        match self {
            TurnToolResources::Captured(tool_set) => tool_set,
            TurnToolResources::Production(config) => config.for_workspace(workspace, task_context),
        }
    }
}

/// The frozen closed composer for one production selection: exactly the enabled builtin
/// definitions/specs pushed by bool in one deterministic order (`ask_user` → `read_file` →
/// `list_directory`, only enabled members, never a duplicate), one frozen planner that
/// routes exactly the three frozen names to exactly one builtin planner each, and the
/// shared `FilesystemRead` outer sandbox contract (the caller composes only when at least
/// one workspace-bound tool is enabled).
///
/// The composition happens at the module planner level, so each enabled workspace-bound
/// route is admitted exactly once against the outer contract (never twice), while the
/// ask_user route never touches admission at all: its plans are UserQuestion/PreExecution
/// shapes carrying no Execute permissions.
fn composed_production_tool_set(
    ask_user: bool,
    read_file: bool,
    list_directory: bool,
    workspace: WorkspaceToolContext,
    task_context: RuntimeTaskContext,
) -> Arc<ToolSet> {
    let mut definitions: Vec<ToolDefinition> = Vec::new();
    if ask_user {
        definitions.push(ask_user::definition());
    }
    if read_file {
        definitions.push(read_file::definition());
    }
    if list_directory {
        definitions.push(list_directory::definition());
    }
    let specs: Arc<[ToolSpec]> = definitions
        .iter()
        .map(|definition| definition.spec.clone())
        .collect();
    let planner: Arc<ToolPlanner> = Arc::new({
        let workspace = workspace.clone();
        let task_context = task_context.clone();
        move |request| match request.call().name().as_str() {
            ask_user::ASK_USER_NAME => ask_user::plan(request),
            read_file::READ_FILE_NAME => read_file::plan(&workspace, &task_context, request),
            list_directory::LIST_DIRECTORY_NAME => {
                list_directory::plan(&workspace, &task_context, request)
            }
            // The normal ToolSet lookup invokes the planner only for a name present in this
            // set's definitions, which are exactly the enabled frozen names above, so no
            // other name can ever reach the composed planner.
            _ => unreachable!(
                "the composed production ToolSet routes exactly the three frozen builtin names"
            ),
        }
    });
    Arc::new(ToolSet {
        inner: Arc::new(ToolSetInner {
            definitions: definitions.into(),
            specs,
            planner: Some(planner),
            sandbox: read_file::sandbox(),
        }),
    })
}

/// The pre-start settlement decision for one resolved Tool approval.
pub(crate) enum ApprovalSettlement {
    /// The host allowed: resume through the exact request's start gate and only then execute
    /// the allowed future.
    Resume,
    /// The host denied: settle as pre-execution without any start-gate reservation.
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

    /// The bounded generic content for one exact tool that never started: the executor was
    /// never polled, so no domain content exists.  Shared by the cancelled-before-start and
    /// failed-before-start outcomes.
    fn did_not_start() -> Self {
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
    /// The private restricted-option decision: resume only after the candidate revalidates
    /// as a subset of the plan ceiling and re-admits against the captured sandbox.
    #[allow(
        dead_code,
        reason = "constructed by M13.3 approval tests (Restricted options) until the M14 production planner"
    )]
    AllowWith(ToolPermissionSet),
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
        // One exact pairing per kind: AsRequested ↔ AllowOnce, Restricted ↔ AllowWith.
        let compatible = matches!(
            (&decision, view.kind()),
            (
                ToolApprovalDecision::AllowOnce,
                ToolApprovalOptionKindView::AsRequested
            ) | (
                ToolApprovalDecision::AllowWith(_),
                ToolApprovalOptionKindView::Restricted
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

    /// Validates the exact-binding invariant that one approval belongs to one exact Tool
    /// request, in normal release code (never `debug_assert`-only): the approval view's tool
    /// name must be the exact request's call name.  A mismatched approval is never
    /// presented; the consuming slot fails closed to the identity-bound Abandoned
    /// OutcomeUnknown outcome without any interaction request, gate, or start factory.
    pub(crate) fn matches_request(&self, request: &ToolExecutionRequest) -> bool {
        self.view().tool_name() == request.call().name()
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
                ToolApprovalDecision::AllowOnce | ToolApprovalDecision::AllowWith(_),
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

/// Test-only fixture for one legitimate live approval request bound to the exact tool name
/// the session tests execute (the echo calls in round tests), so the approval's tool name
/// matches the exact `ToolExecutionRequest` call name.
#[cfg(test)]
pub(crate) fn live_approval_request_fixture_for(tool_name: &str) -> ToolApprovalRequest {
    let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
    let option = ToolApprovalOptionView::new(
        0,
        ToolApprovalOptionKindView::AsRequested,
        "Allow once",
        requirements.clone(),
    )
    .unwrap();
    let view = ToolApprovalRequestView::new(
        tool_name.parse().unwrap(),
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

#[cfg(test)]
pub(crate) fn live_approval_request_fixture() -> ToolApprovalRequest {
    live_approval_request_fixture_for("write_file")
}

/// Test-only fixture for one legitimate live Restricted approval request bound to the exact
/// tool name the session tests execute; its single option resolves to the given `AllowWith`
/// candidate, never the wider plan ceiling.
#[cfg(test)]
pub(crate) fn live_restricted_approval_request_fixture_for(
    tool_name: &str,
    candidate: ToolPermissionSet,
) -> ToolApprovalRequest {
    let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
    let option = ToolApprovalOptionView::new(
        0,
        ToolApprovalOptionKindView::Restricted,
        "Restricted",
        requirements.clone(),
    )
    .unwrap();
    let view = ToolApprovalRequestView::new(
        tool_name.parse().unwrap(),
        "path: src/lib.rs",
        "write requested",
        requirements,
        vec![option.clone()],
    )
    .unwrap();
    ToolApprovalRequest::new(
        view,
        vec![ToolApprovalOption::new(option, ToolApprovalDecision::AllowWith(candidate)).unwrap()],
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    async fn run_planned_execution(
        set: Arc<ToolSet>,
        request: ToolExecutionRequest,
    ) -> ToolExecutionOutcome {
        let ToolExecutionPlan::Execute { start, .. } = set
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
    fn approval_settlement_allows_resume_denies_preexecution_and_cancellation_rewrites_the_denied_result()
     {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let denied_result = ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::from_text_parts(vec!["approval denied".to_owned()])
                .unwrap(),
        };
        // A host AllowOnce with the empty default permission set revalidates against the
        // empty default sandbox and resumes through the exact request's start gate.
        let set = ToolSet::empty();
        assert!(matches!(
            set.approval_settlement(
                &ToolApprovalDecision::AllowOnce,
                ToolPermissionSet::new([]),
                &denied_result,
            ),
            ApprovalSettlement::Resume
        ));
        // A host Deny accepts only a valid PreExecution Denied (preserved exactly) or an
        // explicit Abandoned (reason preserved); every other shape fails closed to Abandoned
        // OutcomeUnknown through the same pre-execution binder.
        assert!(matches!(
            set.approval_settlement(
                &ToolApprovalDecision::Deny,
                ToolPermissionSet::new([]),
                &denied_result,
            ),
            ApprovalSettlement::PreExecution(result)
                if matches!(&result, ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Denied,
                    content,
                } if content.parts()[0].as_text() == "approval denied")
        ));
        assert!(matches!(
            set.approval_settlement(
                &ToolApprovalDecision::Deny,
                ToolPermissionSet::new([]),
                &ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                },
            ),
            ApprovalSettlement::PreExecution(result)
                if matches!(result, ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                })
        ));
        for malformed in [
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Succeeded,
                content: ToolResultContent::from_text_parts(vec!["must not settle".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Failed,
                content: ToolResultContent::from_text_parts(vec!["must not settle".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec!["must not settle".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::completed_text("must not settle").unwrap(),
            ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["must not settle".to_owned()])
                    .unwrap(),
            },
        ] {
            assert!(matches!(
                set.approval_settlement(
                    &ToolApprovalDecision::Deny,
                    ToolPermissionSet::new([]),
                    &malformed,
                ),
                ApprovalSettlement::PreExecution(result)
                    if matches!(result, ToolExecutionResult::Abandoned {
                        reason: ToolAbandonReason::OutcomeUnknown,
                    })
            ));
            // The generic pre-execution binder never fabricates a different outcome from the
            // fail-closed projection.
            assert!(matches!(
                ToolSet::bind_preexecution_result(
                    &request,
                    ToolSet::denied_settlement_result(&malformed)
                ),
                ToolExecutionOutcome::Abandoned {
                    reason: ToolAbandonReason::OutcomeUnknown,
                    ..
                }
            ));
        }
        // The cancellation helper preserves the denied content and rewrites the disposition
        // to Cancelled, so an owner/host-cancelled approval settles a matching cancelled
        // pre-execution under the same pre-execution binding.
        assert!(matches!(
            ToolSet::cancelled_result(denied_result),
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Cancelled,
                content,
            } if content.parts()[0].as_text() == "approval denied"
        ));
        // An explicit Abandoned passes its reason through unchanged.
        assert!(matches!(
            ToolSet::cancelled_result(ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            }),
            ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            }
        ));
        // Every other shape is never rewritten: a PreExecution with any disposition other
        // than Denied, or a malformed Completed shape (including Denied), projects Abandoned
        // OutcomeUnknown so the same pre-execution binding fails it closed (a cancelled
        // approval can never be disguised as an executed ToolResult or as a different
        // pre-execution outcome), and no cancellation is fabricated from a malformed plan.
        for malformed in [
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Succeeded,
                content: ToolResultContent::from_text_parts(vec!["must not rewrite".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Failed,
                content: ToolResultContent::from_text_parts(vec!["must not rewrite".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec!["must not rewrite".to_owned()])
                    .unwrap(),
            },
            ToolExecutionResult::completed_text("must not rewrite").unwrap(),
            ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["must not rewrite".to_owned()])
                    .unwrap(),
            },
        ] {
            assert!(matches!(
                ToolSet::cancelled_result(malformed.clone()),
                ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::OutcomeUnknown,
                }
            ));
            assert!(matches!(
                ToolSet::bind_approval_cancellation(&request, malformed),
                ToolExecutionOutcome::Abandoned {
                    reason: ToolAbandonReason::OutcomeUnknown,
                    ..
                }
            ));
        }
        assert!(matches!(
            ToolSet::bind_approval_cancellation(
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
    fn user_question_answer_binding_is_redacted_and_binds_only_truthful_answered_shapes() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                .unwrap();

        // The binding is move-only and its Debug is fully redacted: neither the answer nor
        // any produced content is reachable through it.
        let binding = UserQuestionAnswerBinding::new(request.clone(), |_| {
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Succeeded,
                content: ToolResultContent::from_text_parts(vec!["SECRET-ANSWER".to_owned()])
                    .unwrap(),
            }
        });
        assert_eq!(format!("{binding:?}"), "UserQuestionAnswerBinding { .. }");
        assert!(matches!(
            binding.bind(&request, answer.clone()),
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == "SECRET-ANSWER"
        ));

        // The producer reads the typed answer through the FnOnce: the binding is the exact
        // owner path that turns the typed host answer into the answered ToolResult.
        let binding = UserQuestionAnswerBinding::new(request.clone(), |answer| {
            let text = match answer.answers()[0].value() {
                UserQuestionAnswerValue::Text(text) => text.as_ref().to_owned(),
                UserQuestionAnswerValue::Choice { .. } => panic!("wrong answer family"),
            };
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Succeeded,
                content: ToolResultContent::from_text_parts(vec![format!("answered: {text}")])
                    .unwrap(),
            }
        });
        assert!(matches!(
            binding.bind(&request, answer.clone()),
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == "answered: yes"
        ));

        // An explicit Abandoned passes its reason through unchanged.
        let binding =
            UserQuestionAnswerBinding::new(request.clone(), |_| ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            });
        assert!(matches!(
            binding.bind(&request, answer.clone()),
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
        ));
    }

    #[test]
    fn user_question_answer_binding_fails_closed_on_malformed_answered_shapes() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                .unwrap();
        // A Completed shape after a question is an invariant: the executor never ran, so an
        // answered question can never be disguised as an executed ToolResult.
        for malformed in [
            ToolExecutionResult::completed_text("ran").unwrap(),
            ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Failed,
                content: ToolResultContent::from_text_parts(vec!["failed".to_owned()]).unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec!["denied".to_owned()]).unwrap(),
            },
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec!["cancelled".to_owned()]).unwrap(),
            },
        ] {
            let binding =
                UserQuestionAnswerBinding::new(request.clone(), move |_| malformed.clone());
            assert!(matches!(
                binding.bind(&request, answer.clone()),
                ToolExecutionOutcome::Abandoned {
                    item_id,
                    tool_call_id,
                    reason: ToolAbandonReason::OutcomeUnknown,
                } if item_id == request.item_id()
                    && tool_call_id == *request.call().tool_call_id()
            ));
        }
    }

    #[test]
    fn user_question_answer_binding_rejects_a_foreign_exact_request_capture_before_invoking() {
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_1".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        let foreign = ToolExecutionRequest::new(
            request.item_id(),
            ToolCall::new(
                request.call().tool_call_id().clone(),
                request.call().name().clone(),
                request.call().arguments().clone(),
                request.call().call_index(),
            ),
        );
        assert!(!request.is_exact_capture(&foreign));
        let invoked = Arc::new(AtomicBool::new(false));
        let binding = UserQuestionAnswerBinding::new(request, {
            let invoked = Arc::clone(&invoked);
            move |_| {
                invoked.store(true, Ordering::Release);
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Succeeded,
                    content: ToolResultContent::from_text_parts(vec!["must not bind".to_owned()])
                        .unwrap(),
                }
            }
        });
        let answer =
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                .unwrap();
        assert!(matches!(
            binding.bind(&foreign, answer),
            ToolExecutionOutcome::Abandoned {
                item_id,
                tool_call_id,
                reason: ToolAbandonReason::RuntimeFailure,
            } if item_id == foreign.item_id()
                && tool_call_id == *foreign.call().tool_call_id()
        ));
        assert!(!invoked.load(Ordering::Acquire));
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
            Some(ToolExecutionPlan::Execute { .. })
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

    use ToolCapabilityClass as C;

    const ALL: [ToolCapabilityClass; 4] = [
        C::FilesystemRead,
        C::FilesystemWrite,
        C::Network,
        C::Process,
    ];

    /// Test-local adapter side filling the small value contract; no trait abstraction.
    struct FakeCapabilityBackend(ToolSandboxContract);

    impl FakeCapabilityBackend {
        fn enforcing(classes: impl IntoIterator<Item = ToolCapabilityClass>) -> Self {
            Self(ToolSandboxContract::available(classes))
        }

        fn unavailable() -> Self {
            Self(ToolSandboxContract::unavailable())
        }

        fn admit(
            &self,
            permissions: ToolPermissionSet,
        ) -> Result<ToolSandboxProof, ToolSandboxAdmissionError> {
            self.0.admit(permissions)
        }
    }

    #[test]
    fn tool_sandbox_available_backend_admits_full_coverage_and_keeps_exact_proof() {
        let rw_net = [C::FilesystemRead, C::FilesystemWrite, C::Network];
        let final_set = ToolPermissionSet::new(rw_net);
        let exact = FakeCapabilityBackend::enforcing(rw_net).admit(final_set);
        let superset = FakeCapabilityBackend::enforcing(ALL).admit(final_set);
        assert_eq!(exact.unwrap().permissions(), final_set);
        assert_eq!(superset.unwrap().permissions(), final_set);
        let gap = FakeCapabilityBackend::enforcing([C::FilesystemRead]).admit(final_set);
        assert!(gap.is_err());
    }

    #[test]
    fn tool_sandbox_gap_is_exact_and_binds_to_a_denied_preexecution_outcome() {
        let required = ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite, C::Network]);
        let error = FakeCapabilityBackend::enforcing([C::FilesystemRead, C::Process])
            .admit(required)
            .unwrap_err();
        let missing = error
            .missing()
            .expect("a gap failure carries its exact missing set");
        let expected = ToolPermissionSet::new([C::FilesystemWrite, C::Network]);
        assert_eq!(missing, expected);
        let request = ToolExecutionRequest::new(
            "itm_00000000000000000000000000000001".parse().unwrap(),
            ToolCall::new(
                "call_sandbox_gap".parse().unwrap(),
                "echo".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(matches!(
            ToolSet::bind_preexecution_result(&request, error.denied_result()),
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Denied,
                ref content,
            } if item_id == request.item_id() && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == TOOL_CAPABILITY_GAP_TEXT
        ));
    }

    #[test]
    fn tool_sandbox_unavailable_fails_closed_even_with_full_coverage() {
        let error = FakeCapabilityBackend::unavailable()
            .admit(ToolPermissionSet::new(ALL))
            .unwrap_err();
        assert!(matches!(error, ToolSandboxAdmissionError::Unavailable));
        assert!(error.missing().is_none());
        assert!(matches!(
            error.denied_result(),
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content,
            } if content.parts()[0].as_text() == TOOL_SANDBOX_UNAVAILABLE_TEXT
        ));
    }

    #[test]
    fn tool_sandbox_restricted_candidate_may_narrow_but_never_widen_the_ceiling() {
        let rw_net = [C::FilesystemRead, C::FilesystemWrite, C::Network];
        let ceiling = ToolPermissionSet::new(rw_net);
        let equal = ceiling.restricted_candidate(rw_net).unwrap();
        let subset = ceiling
            .restricted_candidate(rw_net[..2].iter().copied())
            .unwrap();
        assert_eq!(equal, ceiling);
        assert_eq!(subset, ToolPermissionSet::new(rw_net[..2].iter().copied()));
        let narrowed = ceiling.restricted_candidate([]).unwrap();
        assert!(narrowed.is_empty());
        for elevated in [
            vec![C::FilesystemRead, C::Process],
            ALL.to_vec(),
            vec![C::Process],
        ] {
            assert!(ceiling.restricted_candidate(elevated).is_err());
        }
    }

    #[test]
    fn tool_sandbox_empty_permissions_admit_when_available_but_not_when_unavailable() {
        let empty = ToolPermissionSet::new([]);
        let proof = FakeCapabilityBackend::enforcing([C::FilesystemRead])
            .admit(empty)
            .unwrap();
        assert_eq!(proof.permissions(), empty);
        assert!(FakeCapabilityBackend::unavailable().admit(empty).is_err());
    }

    /// One bounded echo definition shared by the focused sandbox-plan tests.
    fn echo_definition(mode: ToolExecutionMode) -> ToolDefinition {
        ToolDefinition::new(
            "echo".parse().unwrap(),
            "Echo a bounded JSON request",
            "{}".parse().unwrap(),
            mode,
        )
        .unwrap()
    }

    fn sandbox_request(suffix: u8, call: &str, name: &str) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            format!("itm_000000000000000000000000000000{suffix:02x}")
                .parse()
                .unwrap(),
            ToolCall::new(
                call.parse().unwrap(),
                name.parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        )
    }

    /// The focused sandbox-plan rig: one echo definition, one scripted planner/factory
    /// pair producing the given final permissions, and the admission call counters.
    fn sandbox_plan_set(
        mode: ToolExecutionMode,
        sandbox: ToolSandboxContract,
        permissions: ToolPermissionSet,
        response: &'static str,
    ) -> (Arc<ToolSet>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let planner_calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let set = ToolSet::with_sandbox_contract(vec![echo_definition(mode)], sandbox, {
            let planner_calls = Arc::clone(&planner_calls);
            let factory_calls = Arc::clone(&factory_calls);
            move |_request| {
                planner_calls.fetch_add(1, Ordering::Relaxed);
                let factory_calls = Arc::clone(&factory_calls);
                ToolExecutionPlan::Execute {
                    permissions,
                    start: ToolExecutionStart::new(move |_observer| {
                        factory_calls.fetch_add(1, Ordering::Relaxed);
                        Box::pin(
                            async move { ToolExecutionResult::completed_text(response).unwrap() },
                        )
                    }),
                }
            }
        });
        (set, planner_calls, factory_calls)
    }

    #[tokio::test]
    async fn tool_sandbox_plan_admits_and_executes_the_factory_exactly_once() {
        let (set, planner_calls, factory_calls) = sandbox_plan_set(
            ToolExecutionMode::Parallel,
            ToolSandboxContract::available([C::FilesystemRead, C::FilesystemWrite]),
            ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite]),
            "sandboxed echo",
        );
        let request = sandbox_request(0x0c, "call_sandbox_ok", "echo");
        // The admitted Execute plan survives planning and runs through the existing typed
        // start proof/run helper; planner and factory each fire exactly once.
        let outcome = run_planned_execution(Arc::clone(&set), request.clone()).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == "sandboxed echo"
        ));
        assert_eq!(planner_calls.load(Ordering::Relaxed), 1);
        assert_eq!(factory_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn tool_sandbox_plan_gap_denies_without_invoking_the_factory_or_touching_the_gate() {
        let (set, planner_calls, factory_calls) = sandbox_plan_set(
            ToolExecutionMode::Parallel,
            ToolSandboxContract::available([C::FilesystemRead]),
            ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite]),
            "must not run",
        );
        let request = sandbox_request(0x0e, "call_sandbox_gap_plan", "echo");
        // A gate bound to the same exact request exists before planning, exactly like the
        // Session Execution slot's Prepared gate.
        let gate = ToolStartGate::new(request.clone());
        let Some(ToolExecutionPlan::PreExecution(result)) = set.plan(&request) else {
            panic!("a capability gap truthfully denies the Execute plan into PreExecution");
        };
        assert_eq!(
            result,
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec![
                    TOOL_CAPABILITY_GAP_TEXT.to_owned()
                ])
                .unwrap(),
            }
        );
        // The denied frozen result settles through the existing pre-execution binding.
        assert!(matches!(
            ToolSet::bind_preexecution_result(&request, result),
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Denied,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == TOOL_CAPABILITY_GAP_TEXT
        ));
        assert_eq!(planner_calls.load(Ordering::Relaxed), 1);
        assert_eq!(factory_calls.load(Ordering::Relaxed), 0);
        // The untouched gate still accepts its single reservation and start.
        assert!(gate.reserve(&request).unwrap().start().is_ok());
    }

    #[test]
    fn tool_sandbox_plan_unavailable_denies_without_invoking_the_factory_or_touching_the_gate() {
        let (set, planner_calls, factory_calls) = sandbox_plan_set(
            ToolExecutionMode::Serial,
            ToolSandboxContract::unavailable(),
            ToolPermissionSet::new(ALL),
            "must not run",
        );
        let request = sandbox_request(0x0f, "call_sandbox_unavailable_plan", "echo");
        let gate = ToolStartGate::new(request.clone());
        let Some(ToolExecutionPlan::PreExecution(result)) = set.plan(&request) else {
            panic!("an unavailable sandbox truthfully denies the Execute plan");
        };
        assert_eq!(
            result,
            ToolExecutionResult::PreExecution {
                disposition: ToolResultDisposition::Denied,
                content: ToolResultContent::from_text_parts(vec![
                    TOOL_SANDBOX_UNAVAILABLE_TEXT.to_owned()
                ])
                .unwrap(),
            }
        );
        assert!(matches!(
            ToolSet::bind_preexecution_result(&request, result),
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Denied,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts()[0].as_text() == TOOL_SANDBOX_UNAVAILABLE_TEXT
        ));
        assert_eq!(planner_calls.load(Ordering::Relaxed), 1);
        assert_eq!(factory_calls.load(Ordering::Relaxed), 0);
        assert!(gate.reserve(&request).unwrap().start().is_ok());
    }

    #[test]
    fn tool_sandbox_plan_keeps_explicit_preexecution_plans_frozen_even_when_unavailable() {
        let frozen = ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Failed,
            content: ToolResultContent::from_text_parts(vec!["schema preflight failed".to_owned()])
                .unwrap(),
        };
        let set = ToolSet::with_sandbox_contract(
            vec![echo_definition(ToolExecutionMode::Serial)],
            ToolSandboxContract::unavailable(),
            {
                let frozen = frozen.clone();
                move |_request| ToolExecutionPlan::PreExecution(frozen.clone())
            },
        );
        let request = sandbox_request(0x10, "call_frozen_plan", "echo");
        // Only Execute plans are admitted: the explicit frozen plan keeps its original
        // disposition and content even with an unavailable sandbox.
        let Some(ToolExecutionPlan::PreExecution(result)) = set.plan(&request) else {
            panic!("an explicit PreExecution plan is returned unchanged");
        };
        assert_eq!(result, frozen);
    }

    /// The approval-settlement rig: only the captured sandbox contract matters, the planner
    /// is never consulted by the settlement.
    fn sandbox_settlement_set(sandbox: ToolSandboxContract) -> Arc<ToolSet> {
        ToolSet::with_sandbox_contract(vec![], sandbox, |_| {
            unreachable!("approval settlement never consults the planner")
        })
    }

    fn denied_fixture(text: &'static str) -> ToolExecutionResult {
        ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::from_text_parts(vec![text.to_owned()]).unwrap(),
        }
    }

    /// Asserts the settlement fails closed to the fixed generic PreExecution Denied
    /// carrying `text` (the sandbox capability-gap or unavailable reason).
    fn assert_settlement_denied(
        set: &ToolSet,
        decision: ToolApprovalDecision,
        permissions: ToolPermissionSet,
        text: &str,
    ) {
        assert!(matches!(
            set.approval_settlement(&decision, permissions, &denied_fixture("approval denied")),
            ApprovalSettlement::PreExecution(result)
                if matches!(&result, ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Denied,
                    content,
                } if content.parts()[0].as_text() == text)
        ));
    }

    /// Asserts the settlement resumes through the exact start gate.
    fn assert_settlement_resumes(
        set: &ToolSet,
        decision: ToolApprovalDecision,
        permissions: ToolPermissionSet,
    ) {
        assert!(matches!(
            set.approval_settlement(&decision, permissions, &denied_fixture("approval denied")),
            ApprovalSettlement::Resume
        ));
    }

    #[test]
    fn tool_sandbox_approval_restricted_option_revalidates_the_restricted_final_set() {
        let ceiling = ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite, C::Network]);
        let restricted = ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite]);
        // The Restricted option resolves to the exact AllowWith candidate: kind and private
        // decision match the option index.
        let request = live_restricted_approval_request_fixture_for("echo", restricted);
        let (decision, resolution) = request
            .resolve(ToolApprovalDecisionInput::Allow { option_index: 0 })
            .unwrap();
        assert!(matches!(
            decision,
            ToolApprovalDecision::AllowWith(candidate) if candidate == restricted
        ));
        assert!(matches!(
            resolution.as_ref(),
            ToolApprovalResolutionRef::Allowed {
                option_index: 0,
                kind: ToolApprovalOptionKindView::Restricted,
            }
        ));
        // The sandbox enforces only the restricted subset: the AllowWith revalidation admits
        // the candidate and resumes, while the wider as-requested ceiling fails — the
        // restricted final set, not the ceiling, is the admission input.
        let set = sandbox_settlement_set(ToolSandboxContract::available([
            C::FilesystemRead,
            C::FilesystemWrite,
        ]));
        assert_settlement_resumes(&set, ToolApprovalDecision::AllowWith(restricted), ceiling);
        assert_settlement_denied(
            &set,
            ToolApprovalDecision::AllowOnce,
            ceiling,
            TOOL_CAPABILITY_GAP_TEXT,
        );
    }

    #[test]
    fn tool_sandbox_approval_restricted_candidate_cannot_elevate_beyond_the_plan_ceiling() {
        // The candidate adds Network beyond the plan ceiling; even a fully enforcing sandbox
        // fails closed: an approval can never elevate a capability class the plan itself did
        // not carry.
        assert_settlement_denied(
            &sandbox_settlement_set(ToolSandboxContract::available(ALL)),
            ToolApprovalDecision::AllowWith(ToolPermissionSet::new([
                C::FilesystemRead,
                C::Network,
            ])),
            ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite]),
            TOOL_CAPABILITY_GAP_TEXT,
        );
    }

    #[test]
    fn tool_sandbox_approval_allow_once_revalidates_the_captured_sandbox_before_resume() {
        let ceiling = ToolPermissionSet::new([C::FilesystemRead, C::FilesystemWrite]);
        // A capability gap and an unavailable sandbox fail closed with the fixed denied
        // texts; a fully enforcing sandbox resumes through the exact start gate.
        assert_settlement_denied(
            &sandbox_settlement_set(ToolSandboxContract::available([C::FilesystemRead])),
            ToolApprovalDecision::AllowOnce,
            ceiling,
            TOOL_CAPABILITY_GAP_TEXT,
        );
        assert_settlement_denied(
            &sandbox_settlement_set(ToolSandboxContract::unavailable()),
            ToolApprovalDecision::AllowOnce,
            ceiling,
            TOOL_SANDBOX_UNAVAILABLE_TEXT,
        );
        assert_settlement_resumes(
            &sandbox_settlement_set(ToolSandboxContract::available([
                C::FilesystemRead,
                C::FilesystemWrite,
            ])),
            ToolApprovalDecision::AllowOnce,
            ceiling,
        );
    }

    #[test]
    fn tool_sandbox_approval_option_pairing_and_exact_resolution_reject_cross_pairs() {
        let requirements = ToolRequirementSummaryView::new(None, None, None).unwrap();
        let candidate = ToolPermissionSet::new([C::FilesystemRead]);
        let as_requested = ToolApprovalOptionView::new(
            0,
            ToolApprovalOptionKindView::AsRequested,
            "Allow once",
            requirements.clone(),
        )
        .unwrap();
        let restricted = ToolApprovalOptionView::new(
            1,
            ToolApprovalOptionKindView::Restricted,
            "Restricted",
            requirements.clone(),
        )
        .unwrap();
        // The exact pairing: AsRequested maps only to AllowOnce, Restricted only to
        // AllowWith; one two-option request carries both, and every cross-pair is rejected.
        let request = ToolApprovalRequest::new(
            ToolApprovalRequestView::new(
                "echo".parse().unwrap(),
                "path: src/lib.rs",
                "write requested",
                requirements,
                vec![as_requested.clone(), restricted.clone()],
            )
            .unwrap(),
            vec![
                ToolApprovalOption::new(as_requested.clone(), ToolApprovalDecision::AllowOnce)
                    .unwrap(),
                ToolApprovalOption::new(
                    restricted.clone(),
                    ToolApprovalDecision::AllowWith(candidate),
                )
                .unwrap(),
            ],
        )
        .unwrap();
        for (view, decision) in [
            (
                as_requested.clone(),
                ToolApprovalDecision::AllowWith(candidate),
            ),
            (restricted.clone(), ToolApprovalDecision::AllowOnce),
        ] {
            assert!(matches!(
                ToolApprovalOption::new(view, decision),
                Err(ToolValueError::InvalidApproval)
            ));
        }
        // The exact resolution validation admits only the paired kind/index/decision; any
        // cross-pair of the two options is invalid.
        for (decision, index, kind, valid) in [
            (
                ToolApprovalDecision::AllowOnce,
                0u32,
                ToolApprovalOptionKindView::AsRequested,
                true,
            ),
            (
                ToolApprovalDecision::AllowWith(candidate),
                1,
                ToolApprovalOptionKindView::Restricted,
                true,
            ),
            (
                ToolApprovalDecision::AllowWith(candidate),
                0,
                ToolApprovalOptionKindView::AsRequested,
                false,
            ),
            (
                ToolApprovalDecision::AllowOnce,
                1,
                ToolApprovalOptionKindView::Restricted,
                false,
            ),
            (
                ToolApprovalDecision::AllowWith(candidate),
                1,
                ToolApprovalOptionKindView::AsRequested,
                false,
            ),
        ] {
            assert_eq!(
                request
                    .validate_exact_resolution(
                        &decision,
                        &ToolApprovalResolution::reconstruct_allowed(index, kind),
                    )
                    .is_ok(),
                valid
            );
        }
    }

    // ---------- ProductionToolConfig frozen composition ----------

    use std::num::NonZeroU64;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;

    use crate::wire::{CanonicalFileUri, SessionId, WorkspaceRevision};
    use crate::workspace::{
        RequestedFilesystemAccess, Workspace, WorkspaceCwdSpec, WorkspaceDefinitionInput,
        WorkspacePathTarget, WorkspaceResolver, WorkspaceRootInput, WorkspaceSourcePolicy,
        lower_workspace,
    };

    const COMPOSITION_SESSION_ID: &str = "ses_22222222222222222222222222222222";

    static NEXT_COMPOSITION_TEMP_SUFFIX: AtomicU64 = AtomicU64::new(1);

    struct CompositionTempDir(PathBuf);

    impl CompositionTempDir {
        fn new(label: &str) -> Self {
            let suffix = NEXT_COMPOSITION_TEMP_SUFFIX.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "minicore-runtime-tools-composition-{label}-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("the test temporary directory is creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for CompositionTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_fixture(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture parent is creatable");
        }
        std::fs::write(path, bytes).expect("the fixture file is writable");
    }

    fn composition_session_id() -> SessionId {
        COMPOSITION_SESSION_ID
            .parse()
            .expect("the composition test session id is canonical")
    }

    /// Lowers one real temp-dir Workspace (one primary root, cwd at `cwd_relative` inside
    /// it) through the production lowering path — the same Workspace test helpers the
    /// read_file module tests use.
    fn composition_workspace_spec(root: &Path, cwd_relative: &str) -> Workspace {
        let uri: CanonicalFileUri = format!("file://{}", root.display())
            .parse()
            .expect("the test root is a canonical native file URI");
        let root_input = WorkspaceRootInput::new(
            "primary".parse().expect("the test root key is canonical"),
            uri,
            RequestedFilesystemAccess::ReadOnly,
            WorkspaceSourcePolicy::new(false, false),
        );
        let input = WorkspaceDefinitionInput::new(
            root_input,
            Vec::new(),
            WorkspaceCwdSpec::new(
                "primary".parse().expect("the test root key is canonical"),
                cwd_relative
                    .parse()
                    .expect("the test cwd relative path is canonical"),
            ),
        )
        .expect("the test workspace definition is valid");
        lower_workspace(
            input,
            WorkspaceRevision::new(NonZeroU64::new(1).expect("the test revision is non-zero")),
            WorkspacePathTarget::Posix,
        )
        .expect("the test workspace lowers")
    }

    /// Resolves the workspace through the resolver into the published snapshot's tool
    /// context.
    async fn resolve_composition_context(
        resolver: WorkspaceResolver,
        workspace: Workspace,
    ) -> WorkspaceToolContext {
        let candidate = resolver
            .resolve(composition_session_id(), &workspace)
            .await
            .expect("the test workspace resolves");
        let snapshot = candidate
            .finish(Arc::from(Vec::new()), Arc::from(Vec::new()))
            .expect("the test snapshot finishes");
        snapshot.tool_context()
    }

    /// One real temp-dir root lowered to a readable-grant tool context with one fixture
    /// file, plus the live task context the selection pins.
    struct CompositionWorkspace {
        _temporary: CompositionTempDir,
        task_context: RuntimeTaskContext,
        workspace: WorkspaceToolContext,
    }

    async fn composition_workspace() -> CompositionWorkspace {
        let temporary = CompositionTempDir::new("selection");
        write_fixture(temporary.path(), "notes.txt", b"body");
        let task_context = RuntimeTaskContext::new(tokio::runtime::Handle::current())
            .await
            .expect("the test runtime provides tracked task admission");
        let resolver =
            WorkspaceResolver::new_with_source_grants_for_test(task_context.clone(), false, false);
        let workspace =
            resolve_composition_context(resolver, composition_workspace_spec(temporary.path(), ""))
                .await;
        CompositionWorkspace {
            _temporary: temporary,
            task_context,
            workspace,
        }
    }

    fn composition_request(
        suffix: u8,
        call: &str,
        name: &str,
        arguments: &str,
    ) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            format!("itm_000000000000000000000000000000{suffix:02x}")
                .parse()
                .unwrap(),
            ToolCall::new(
                call.parse().unwrap(),
                name.parse().unwrap(),
                arguments.parse().unwrap(),
                0,
            ),
        )
    }

    #[derive(Clone, Copy, Debug)]
    struct Selection {
        ask_user: bool,
        read_file: bool,
        list_directory: bool,
        names: &'static [&'static str],
    }

    /// The closed eight-selection surface: every combination of the three frozen builtin
    /// opt-ins, with the exact expected ordered names (the fixed definition/spec order
    /// `ask_user` → `read_file` → `list_directory`, only enabled members, no duplicates).
    const SELECTIONS: [Selection; 8] = [
        Selection {
            ask_user: false,
            read_file: false,
            list_directory: false,
            names: &[],
        },
        Selection {
            ask_user: true,
            read_file: false,
            list_directory: false,
            names: &[ask_user::ASK_USER_NAME],
        },
        Selection {
            ask_user: false,
            read_file: true,
            list_directory: false,
            names: &[read_file::READ_FILE_NAME],
        },
        Selection {
            ask_user: false,
            read_file: false,
            list_directory: true,
            names: &[list_directory::LIST_DIRECTORY_NAME],
        },
        Selection {
            ask_user: true,
            read_file: true,
            list_directory: false,
            names: &[ask_user::ASK_USER_NAME, read_file::READ_FILE_NAME],
        },
        Selection {
            ask_user: true,
            read_file: false,
            list_directory: true,
            names: &[ask_user::ASK_USER_NAME, list_directory::LIST_DIRECTORY_NAME],
        },
        Selection {
            ask_user: false,
            read_file: true,
            list_directory: true,
            names: &[
                read_file::READ_FILE_NAME,
                list_directory::LIST_DIRECTORY_NAME,
            ],
        },
        Selection {
            ask_user: true,
            read_file: true,
            list_directory: true,
            names: &[
                ask_user::ASK_USER_NAME,
                read_file::READ_FILE_NAME,
                list_directory::LIST_DIRECTORY_NAME,
            ],
        },
    ];

    #[tokio::test(flavor = "current_thread")]
    async fn production_config_all_eight_selections_freeze_exact_names_specs_reuse_and_sandbox() {
        let rig = composition_workspace().await;
        for selection in SELECTIONS {
            let config = ProductionToolConfig::new(
                selection.ask_user,
                selection.read_file,
                selection.list_directory,
            );
            let set = config.for_workspace(rig.workspace.clone(), rig.task_context.clone());

            // Exactly the enabled definitions and prompt specs in the one frozen order,
            // with no duplicates.
            let definitions = set.definitions();
            assert_eq!(
                definitions.len(),
                selection.names.len(),
                "selection {selection:?}"
            );
            for (definition, name) in definitions.iter().zip(selection.names) {
                assert_eq!(definition.name().as_str(), *name, "selection {selection:?}");
            }
            let view = set.prompt_view();
            assert_eq!(
                view.specs().len(),
                selection.names.len(),
                "selection {selection:?}"
            );
            for (spec, name) in view.specs().iter().zip(selection.names) {
                assert_eq!(spec.name().as_str(), *name, "selection {selection:?}");
            }

            // The outer sandbox contract is available exactly for FilesystemRead when any
            // workspace-bound tool is enabled, and the empty available contract otherwise.
            let has_workspace_tools = selection.read_file || selection.list_directory;
            let expected_sandbox = if has_workspace_tools {
                ToolSandboxContract::available([C::FilesystemRead])
            } else {
                ToolSandboxContract::available([])
            };
            assert_eq!(
                set.inner.sandbox, expected_sandbox,
                "selection {selection:?}"
            );

            // Arc reuse: no workspace-bound tool means every materialization returns the
            // same captured base Arc; any workspace-bound tool means a fresh Arc per
            // admission, bound to the exact Workspace/task contexts.
            let again = config.for_workspace(rig.workspace.clone(), rig.task_context.clone());
            if has_workspace_tools {
                assert!(!Arc::ptr_eq(&set, &again), "selection {selection:?}");
                assert_eq!(
                    again.definitions().len(),
                    selection.names.len(),
                    "selection {selection:?}"
                );
            } else {
                assert!(Arc::ptr_eq(&set, &again), "selection {selection:?}");
            }
        }
        rig.task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn production_config_routes_plan_exactly_once_in_the_selections_that_include_them() {
        let rig = composition_workspace().await;
        for selection in SELECTIONS {
            let config = ProductionToolConfig::new(
                selection.ask_user,
                selection.read_file,
                selection.list_directory,
            );
            let set = config.for_workspace(rig.workspace.clone(), rig.task_context.clone());
            let enabled = |name: &'static str| selection.names.contains(&name);

            // ask_user route: exactly its typed UserQuestion when selected (invalid
            // arguments settle the frozen Failed pre-execution), unavailable otherwise.
            let ask = composition_request(
                0x21,
                "call_ask",
                ask_user::ASK_USER_NAME,
                r#"{"questions":[{"questionIndex":0,"prompt":"Continue?","required":true,"input":{"type":"text","data":{"multiline":false}}}]}"#,
            );
            if enabled(ask_user::ASK_USER_NAME) {
                assert!(
                    matches!(set.plan(&ask), Some(ToolExecutionPlan::UserQuestion { .. })),
                    "selection {selection:?}"
                );
                let invalid =
                    composition_request(0x22, "call_ask_invalid", ask_user::ASK_USER_NAME, "{}");
                assert!(
                    matches!(
                        set.plan(&invalid),
                        Some(ToolExecutionPlan::PreExecution(result))
                            if matches!(result, ToolExecutionResult::PreExecution {
                                disposition: ToolResultDisposition::Failed,
                                ..
                            })
                    ),
                    "selection {selection:?}"
                );
            } else {
                assert!(set.plan(&ask).is_none(), "selection {selection:?}");
            }

            // read_file route: the file path plans Execute with exactly FilesystemRead
            // and survives the single outer admission, and the root path settles the
            // frozen Denied pre-execution; unavailable otherwise.  Each call runs through
            // ToolSet::plan exactly once, whose one admit against the FilesystemRead outer
            // contract is exactly what the returned Execute shape proves: there is no
            // second admission anywhere.
            let read = composition_request(
                0x23,
                "call_read",
                read_file::READ_FILE_NAME,
                r#"{"path":"notes.txt"}"#,
            );
            if enabled(read_file::READ_FILE_NAME) {
                match set.plan(&read) {
                    Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                        assert_eq!(
                            permissions,
                            ToolPermissionSet::new([C::FilesystemRead]),
                            "selection {selection:?}"
                        );
                    }
                    _ => panic!("the valid read_file call plans an Execute shape ({selection:?})"),
                }
                let read_root = composition_request(
                    0x24,
                    "call_root",
                    read_file::READ_FILE_NAME,
                    r#"{"path":""}"#,
                );
                assert!(
                    matches!(
                        set.plan(&read_root),
                        Some(ToolExecutionPlan::PreExecution(result))
                            if matches!(result, ToolExecutionResult::PreExecution {
                                disposition: ToolResultDisposition::Denied,
                                ..
                            })
                    ),
                    "selection {selection:?}"
                );
            } else {
                assert!(set.plan(&read).is_none(), "selection {selection:?}");
            }

            // list_directory route: the empty cwd plans Execute with exactly
            // FilesystemRead through the same single outer admission; unavailable
            // otherwise.
            let list = composition_request(
                0x25,
                "call_list",
                list_directory::LIST_DIRECTORY_NAME,
                r#"{"path":""}"#,
            );
            if enabled(list_directory::LIST_DIRECTORY_NAME) {
                match set.plan(&list) {
                    Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                        assert_eq!(
                            permissions,
                            ToolPermissionSet::new([C::FilesystemRead]),
                            "selection {selection:?}"
                        );
                    }
                    _ => panic!(
                        "the valid list_directory call plans an Execute shape ({selection:?})"
                    ),
                }
            } else {
                assert!(set.plan(&list).is_none(), "selection {selection:?}");
            }

            // A name that is not one of the three frozen builtins stays unavailable
            // through the normal ToolSet lookup in every selection: there is no generic
            // registry or open-world route.
            let unknown = composition_request(0x26, "call_other", "other_tool", "{}");
            assert!(set.plan(&unknown).is_none(), "selection {selection:?}");
        }
        rig.task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_tool_resources_captured_materialize_returns_the_same_arc() {
        let rig = composition_workspace().await;
        let set = ToolSet::ask_user_builtin();
        let materialized = TurnToolResources::Captured(Arc::clone(&set))
            .materialize(rig.workspace.clone(), rig.task_context.clone());
        // A captured carrier returns its exact Arc unchanged, untouched by the Workspace
        // or task context and by any later reload.
        assert!(Arc::ptr_eq(&set, &materialized));
        assert_eq!(materialized.definitions().len(), 1);
        assert_eq!(
            materialized.definitions()[0].name().as_str(),
            ask_user::ASK_USER_NAME
        );
        rig.task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_tool_resources_production_materialize_installs_the_workspace_bound_read_file_set()
    {
        let rig = composition_workspace().await;
        let set = TurnToolResources::Production(ProductionToolConfig::new(false, true, false))
            .materialize(rig.workspace.clone(), rig.task_context.clone());
        // The production read_file selection materializes the exact workspace-bound set:
        // one read_file definition and prompt spec with the FilesystemRead outer contract.
        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name().as_str(), read_file::READ_FILE_NAME);
        assert_eq!(set.prompt_view().specs().len(), 1);
        assert_eq!(
            set.inner.sandbox,
            ToolSandboxContract::available([C::FilesystemRead])
        );
        rig.task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_tool_resources_production_materialize_installs_the_workspace_bound_list_directory_set()
     {
        let rig = composition_workspace().await;
        let set = TurnToolResources::Production(ProductionToolConfig::new(false, false, true))
            .materialize(rig.workspace.clone(), rig.task_context.clone());
        // The production list_directory selection materializes the exact workspace-bound
        // set: one list_directory definition and prompt spec with the FilesystemRead outer
        // contract, and its empty-cwd route plans Execute with exactly FilesystemRead.
        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(
            definitions[0].name().as_str(),
            list_directory::LIST_DIRECTORY_NAME
        );
        assert_eq!(set.prompt_view().specs().len(), 1);
        assert_eq!(
            set.inner.sandbox,
            ToolSandboxContract::available([C::FilesystemRead])
        );
        let list = composition_request(
            0x31,
            "call_list",
            list_directory::LIST_DIRECTORY_NAME,
            r#"{"path":""}"#,
        );
        assert!(
            matches!(set.plan(&list), Some(ToolExecutionPlan::Execute { .. })),
            "the materialized list_directory set plans its Execute route"
        );
        rig.task_context.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn turn_tool_resources_production_materialize_installs_the_composed_read_and_list_set() {
        let rig = composition_workspace().await;
        let set = TurnToolResources::Production(ProductionToolConfig::new(false, true, true))
            .materialize(rig.workspace.clone(), rig.task_context.clone());
        // The production read_file + list_directory selection materializes one composed
        // workspace-bound set in the frozen order, both routes planning Execute with the
        // same FilesystemRead outer contract.
        let definitions = set.definitions();
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0].name().as_str(), read_file::READ_FILE_NAME);
        assert_eq!(
            definitions[1].name().as_str(),
            list_directory::LIST_DIRECTORY_NAME
        );
        assert_eq!(set.prompt_view().specs().len(), 2);
        assert_eq!(
            set.inner.sandbox,
            ToolSandboxContract::available([C::FilesystemRead])
        );
        let read = composition_request(
            0x32,
            "call_read",
            read_file::READ_FILE_NAME,
            r#"{"path":"notes.txt"}"#,
        );
        let list = composition_request(
            0x33,
            "call_list",
            list_directory::LIST_DIRECTORY_NAME,
            r#"{"path":""}"#,
        );
        for request in [&read, &list] {
            assert!(
                matches!(
                    set.plan(request),
                    Some(ToolExecutionPlan::Execute { permissions, .. })
                        if permissions == ToolPermissionSet::new([C::FilesystemRead])
                ),
                "each composed workspace-bound route plans Execute with exactly FilesystemRead"
            );
        }
        rig.task_context.shutdown().await;
    }
}
