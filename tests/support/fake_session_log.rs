use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use minicore_runtime::config::SessionManifest;
use minicore_runtime::conversation::{ConversationEntry, ConversationSeq};
use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::storage::{
    AppendReceipt, ConversationPage, LogFuture, SessionLog, SessionLogError, SessionLogErrorKind,
};
use minicore_runtime::value::BoundedText;

#[derive(Clone, Debug)]
pub enum Script {
    Error(SessionLogErrorKind),
    UnknownOutcome { committed: bool },
    Delay(Duration),
    Panic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Initialize,
    LoadManifest,
    ReadPage {
        after: Option<ConversationSeq>,
        limit: usize,
    },
    Append {
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    },
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FakeSessionLogInitError {
    NonContiguous {
        expected: ConversationSeq,
        actual: ConversationSeq,
    },
}

#[derive(Default)]
struct State {
    manifest: Option<SessionManifest>,
    entries: Vec<ConversationEntry>,
    closed: bool,
    corrupt: bool,
    close_count: usize,
    operations: Vec<Operation>,
    active_mutable_operations: usize,
    max_concurrent_mutable_operations: usize,
    initialize_scripts: VecDeque<Script>,
    load_manifest_scripts: VecDeque<Script>,
    append_scripts: VecDeque<Script>,
    read_scripts: VecDeque<Script>,
    close_scripts: VecDeque<Script>,
}

#[derive(Clone)]
pub struct InspectionHandle {
    state: Arc<Mutex<State>>,
}

impl InspectionHandle {
    pub fn operations(&self) -> Vec<Operation> {
        lock_state(&self.state).operations.clone()
    }

    pub fn entries(&self) -> Vec<ConversationEntry> {
        lock_state(&self.state).entries.clone()
    }

    pub fn manifest(&self) -> Option<SessionManifest> {
        lock_state(&self.state).manifest.clone()
    }

    pub fn close_count(&self) -> usize {
        lock_state(&self.state).close_count
    }

    pub fn max_concurrent_mutable_operations(&self) -> usize {
        lock_state(&self.state).max_concurrent_mutable_operations
    }

    pub fn head(&self) -> ConversationSeq {
        current_head(&lock_state(&self.state))
    }
}

pub struct FakeSessionLog {
    state: Arc<Mutex<State>>,
}

impl Default for FakeSessionLog {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

impl FakeSessionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_initial(
        manifest: SessionManifest,
        entries: Vec<ConversationEntry>,
    ) -> Result<Self, FakeSessionLogInitError> {
        validate_contiguous(&entries)?;
        let log = Self::new();
        let mut state = lock_state(&log.state);
        state.manifest = Some(manifest);
        state.entries = entries;
        drop(state);
        Ok(log)
    }

    pub fn inspection(&self) -> InspectionHandle {
        InspectionHandle {
            state: Arc::clone(&self.state),
        }
    }

    pub fn mark_corrupt(&mut self) {
        lock_state(&self.state).corrupt = true;
    }

    pub fn script_initialize(&mut self, script: Script) {
        lock_state(&self.state).initialize_scripts.push_back(script);
    }

    pub fn script_load_manifest(&mut self, script: Script) {
        lock_state(&self.state)
            .load_manifest_scripts
            .push_back(script);
    }

    pub fn script_append(&mut self, script: Script) {
        lock_state(&self.state).append_scripts.push_back(script);
    }

    pub fn script_read(&mut self, script: Script) {
        lock_state(&self.state).read_scripts.push_back(script);
    }

    pub fn script_close(&mut self, script: Script) {
        lock_state(&self.state).close_scripts.push_back(script);
    }
}

fn lock_state(state: &Arc<Mutex<State>>) -> MutexGuard<'_, State> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone, Copy)]
enum ScriptQueue {
    Initialize,
    LoadManifest,
    Append,
    Read,
    Close,
}

fn begin_operation(
    state: &Arc<Mutex<State>>,
    operation: Operation,
    queue: ScriptQueue,
) -> (Arc<Mutex<State>>, Option<Script>, ActiveOperation) {
    let mut inner = lock_state(state);
    if matches!(operation, Operation::Close) {
        inner.close_count += 1;
    }
    inner.operations.push(operation);
    inner.active_mutable_operations += 1;
    inner.max_concurrent_mutable_operations = inner
        .max_concurrent_mutable_operations
        .max(inner.active_mutable_operations);
    let script = match queue {
        ScriptQueue::Initialize => inner.initialize_scripts.pop_front(),
        ScriptQueue::LoadManifest => inner.load_manifest_scripts.pop_front(),
        ScriptQueue::Append => inner.append_scripts.pop_front(),
        ScriptQueue::Read => inner.read_scripts.pop_front(),
        ScriptQueue::Close => inner.close_scripts.pop_front(),
    };
    drop(inner);
    let owned_state = Arc::clone(state);
    let active = ActiveOperation {
        state: Arc::clone(&owned_state),
    };
    (owned_state, script, active)
}

struct ActiveOperation {
    state: Arc<Mutex<State>>,
}

impl Drop for ActiveOperation {
    fn drop(&mut self) {
        let mut state = lock_state(&self.state);
        state.active_mutable_operations = state.active_mutable_operations.saturating_sub(1);
    }
}

#[derive(Clone, Copy)]
enum ScriptOutcome {
    Continue,
    Error(SessionLogErrorKind),
    UnknownOutcome { committed: bool },
}

async fn run_script(script: Option<Script>) -> ScriptOutcome {
    match script {
        None => ScriptOutcome::Continue,
        Some(Script::Error(kind)) => ScriptOutcome::Error(kind),
        Some(Script::UnknownOutcome { committed }) => ScriptOutcome::UnknownOutcome { committed },
        Some(Script::Delay(duration)) => {
            tokio::time::sleep(duration).await;
            ScriptOutcome::Continue
        }
        Some(Script::Panic) => panic!("scripted fake SessionLog panic"),
    }
}

fn error(kind: SessionLogErrorKind) -> SessionLogError {
    let code = match kind {
        SessionLogErrorKind::Conflict => DiagnosticCode::LogConflict,
        SessionLogErrorKind::Corrupt => DiagnosticCode::LogCorrupt,
        SessionLogErrorKind::UnknownOutcome => DiagnosticCode::LogUnknownOutcome,
        SessionLogErrorKind::NotInitialized | SessionLogErrorKind::AlreadyInitialized => {
            DiagnosticCode::InvalidSessionManifest
        }
        SessionLogErrorKind::Closed => DiagnosticCode::SessionClosed,
        SessionLogErrorKind::Unavailable | SessionLogErrorKind::Internal => {
            DiagnosticCode::Internal
        }
    };
    let message = BoundedText::new(format!("fake session log {kind:?}")).unwrap();
    SessionLogError::new(
        kind,
        DiagnosticSummary::new(code, DiagnosticCategory::Storage, message, false),
    )
}

fn current_head(state: &State) -> ConversationSeq {
    state
        .entries
        .last()
        .map(ConversationEntry::seq)
        .unwrap_or(ConversationSeq::ZERO)
}

fn validate_contiguous(entries: &[ConversationEntry]) -> Result<(), FakeSessionLogInitError> {
    let mut expected = ConversationSeq::ZERO.next();
    for entry in entries {
        if entry.seq() != expected {
            return Err(FakeSessionLogInitError::NonContiguous {
                expected,
                actual: entry.seq(),
            });
        }
        expected = expected.next();
    }
    Ok(())
}

fn append_batch(
    state: &mut State,
    expected_head: ConversationSeq,
    entries: Vec<ConversationEntry>,
) -> Result<AppendReceipt, SessionLogErrorKind> {
    if state.closed {
        return Err(SessionLogErrorKind::Closed);
    }
    if state.corrupt {
        return Err(SessionLogErrorKind::Corrupt);
    }
    if state.manifest.is_none() {
        return Err(SessionLogErrorKind::NotInitialized);
    }
    if expected_head != current_head(state) {
        return Err(SessionLogErrorKind::Conflict);
    }
    if entries.is_empty() {
        return Err(SessionLogErrorKind::Internal);
    }
    let first_expected = expected_head.next();
    let contiguous = entries.first().is_some_and(|entry| {
        entry.seq() == first_expected
            && entries
                .windows(2)
                .all(|pair| pair[1].seq() == pair[0].seq().next())
    });
    if !contiguous {
        return Err(SessionLogErrorKind::Conflict);
    }
    let appended = entries.len();
    let new_head = entries.last().map(ConversationEntry::seq).unwrap();
    state.entries.extend(entries);
    Ok(AppendReceipt {
        previous_head: expected_head,
        new_head,
        appended,
    })
}

impl SessionLog for FakeSessionLog {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        let (state, script, active) =
            begin_operation(&self.state, Operation::Initialize, ScriptQueue::Initialize);
        Box::pin(async move {
            let _active = active;
            match run_script(script).await {
                ScriptOutcome::Error(kind) => Err(error(kind)),
                ScriptOutcome::UnknownOutcome { .. } => {
                    Err(error(SessionLogErrorKind::UnknownOutcome))
                }
                ScriptOutcome::Continue => {
                    let mut state = lock_state(&state);
                    if state.closed {
                        Err(error(SessionLogErrorKind::Closed))
                    } else if state.corrupt {
                        Err(error(SessionLogErrorKind::Corrupt))
                    } else if state.manifest.is_some() {
                        Err(error(SessionLogErrorKind::AlreadyInitialized))
                    } else {
                        state.manifest = Some(manifest);
                        Ok(ConversationSeq::ZERO)
                    }
                }
            }
        })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        let (state, script, active) = begin_operation(
            &self.state,
            Operation::LoadManifest,
            ScriptQueue::LoadManifest,
        );
        Box::pin(async move {
            let _active = active;
            match run_script(script).await {
                ScriptOutcome::Error(kind) => Err(error(kind)),
                ScriptOutcome::UnknownOutcome { .. } => {
                    Err(error(SessionLogErrorKind::UnknownOutcome))
                }
                ScriptOutcome::Continue => {
                    let state = lock_state(&state);
                    if state.closed {
                        Err(error(SessionLogErrorKind::Closed))
                    } else if state.corrupt {
                        Err(error(SessionLogErrorKind::Corrupt))
                    } else {
                        state
                            .manifest
                            .clone()
                            .ok_or_else(|| error(SessionLogErrorKind::NotInitialized))
                    }
                }
            }
        })
    }

    fn read_page<'a>(
        &'a mut self,
        after: Option<ConversationSeq>,
        limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        let (state, script, active) = begin_operation(
            &self.state,
            Operation::ReadPage { after, limit },
            ScriptQueue::Read,
        );
        Box::pin(async move {
            let _active = active;
            match run_script(script).await {
                ScriptOutcome::Error(kind) => Err(error(kind)),
                ScriptOutcome::UnknownOutcome { .. } => {
                    Err(error(SessionLogErrorKind::UnknownOutcome))
                }
                ScriptOutcome::Continue => {
                    let state = lock_state(&state);
                    if state.closed {
                        return Err(error(SessionLogErrorKind::Closed));
                    }
                    if state.corrupt {
                        return Err(error(SessionLogErrorKind::Corrupt));
                    }
                    if state.manifest.is_none() {
                        return Err(error(SessionLogErrorKind::NotInitialized));
                    }
                    let start = after.map_or(0, |value| {
                        state
                            .entries
                            .iter()
                            .position(|entry| entry.seq().get() > value.get())
                            .unwrap_or(state.entries.len())
                    });
                    let end = start.saturating_add(limit).min(state.entries.len());
                    let entries = state.entries[start..end].to_vec();
                    let observed_head = current_head(&state);
                    let next_after = entries
                        .last()
                        .map(ConversationEntry::seq)
                        .filter(|value| value.get() < observed_head.get());
                    Ok(ConversationPage {
                        entries,
                        next_after,
                        observed_head,
                    })
                }
            }
        })
    }

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        let operation_entries = entries.clone();
        let (state, script, active) = begin_operation(
            &self.state,
            Operation::Append {
                expected_head,
                entries: operation_entries,
            },
            ScriptQueue::Append,
        );
        Box::pin(async move {
            let _active = active;
            match run_script(script).await {
                ScriptOutcome::Error(kind) => Err(error(kind)),
                ScriptOutcome::UnknownOutcome { committed } => {
                    if committed {
                        let mut state = lock_state(&state);
                        let _ = append_batch(&mut state, expected_head, entries);
                    }
                    Err(error(SessionLogErrorKind::UnknownOutcome))
                }
                ScriptOutcome::Continue => {
                    let mut state = lock_state(&state);
                    append_batch(&mut state, expected_head, entries).map_err(error)
                }
            }
        })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        let (state, script, active) =
            begin_operation(&self.state, Operation::Close, ScriptQueue::Close);
        Box::pin(async move {
            let _active = active;
            match run_script(script).await {
                ScriptOutcome::Error(kind) => Err(error(kind)),
                ScriptOutcome::UnknownOutcome { .. } => {
                    Err(error(SessionLogErrorKind::UnknownOutcome))
                }
                ScriptOutcome::Continue => {
                    let mut state = lock_state(&state);
                    if state.closed {
                        Err(error(SessionLogErrorKind::Closed))
                    } else {
                        state.closed = true;
                        Ok(())
                    }
                }
            }
        })
    }
}
