use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::compaction::{
    CompactionError, CompactionFuture, CompactionProposal, CompactionRequest, CompactionStrategy,
};
use crate::conversation::{SummaryDraft, SummaryEntry, TurnTerminal, TurnTerminalEntry};

pub(super) enum CompactionBehavior {
    Proposal(CompactionProposal),
    Error(CompactionError),
    Pending,
}

pub(super) struct ScriptCompaction {
    behaviors: Mutex<VecDeque<CompactionBehavior>>,
    requests: Mutex<Vec<CompactionRequest>>,
    calls: AtomicUsize,
    called: Notify,
}

impl ScriptCompaction {
    pub(super) fn new(behaviors: Vec<CompactionBehavior>) -> Arc<Self> {
        Arc::new(Self {
            behaviors: Mutex::new(behaviors.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            called: Notify::new(),
        })
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(super) fn requests(&self) -> Vec<CompactionRequest> {
        lock(&self.requests).clone()
    }

    pub(super) async fn wait_called(&self) {
        loop {
            let notified = self.called.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.calls() > 0 {
                return;
            }
            notified.await;
        }
    }
}

impl CompactionStrategy for ScriptCompaction {
    fn compact<'a>(&'a self, request: CompactionRequest) -> CompactionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request);
        self.called.notify_waiters();
        match lock(&self.behaviors)
            .pop_front()
            .unwrap_or(CompactionBehavior::Error(CompactionError::Internal))
        {
            CompactionBehavior::Proposal(proposal) => Box::pin(async move { Ok(proposal) }),
            CompactionBehavior::Error(error) => Box::pin(async move { Err(error) }),
            CompactionBehavior::Pending => {
                Box::pin(pending::<Result<CompactionProposal, CompactionError>>())
            }
        }
    }
}

pub(super) fn proposal(through: u64, text: &str) -> CompactionProposal {
    CompactionProposal {
        through_seq: ConversationSeq::new(through),
        summary: BoundedText::new(text).unwrap(),
    }
}

pub(super) fn enabled_spec(
    tool_names: &[&str],
    max_tool_rounds: u16,
    trigger_tokens: u64,
    target_tokens: u64,
) -> SessionSpec {
    let mut spec = session_spec(tool_names, max_tool_rounds);
    spec.compaction = CompactionConfig::Enabled {
        trigger_tokens,
        target_tokens,
    };
    spec
}

pub(super) fn bindings_with_compaction(
    model: Arc<ScriptModel>,
    context: Option<Arc<ScriptContext>>,
    tools: Vec<Arc<ScriptTool>>,
    policy: Option<Arc<dyn ToolPolicy>>,
    compaction: Arc<ScriptCompaction>,
) -> SessionBindings {
    let mut bindings = session_bindings(model, context, tools, policy);
    let strategy: Arc<dyn CompactionStrategy> = compaction;
    bindings.compaction = Some(strategy);
    bindings
}

pub(super) fn active_conversation(
    spec: &SessionSpec,
    effective_max_tool_rounds: u16,
    prior_text: &str,
) -> ConversationView {
    let prior: TurnId = "trn_00000000000000000000000000000082".parse().unwrap();
    let entries = vec![
        ConversationEntry::UserMessage(UserMessageEntry {
            seq: ConversationSeq::new(1),
            turn_id: prior,
            input: UserInputRecord::new(BoundedText::new(prior_text).unwrap()).unwrap(),
            execution: TurnExecutionRecord::new(spec.model.clone(), spec.reasoning, 1).unwrap(),
            created_at: timestamp(),
        }),
        ConversationEntry::AssistantMessage(AssistantMessageEntry {
            seq: ConversationSeq::new(2),
            turn_id: prior,
            model: spec.model.clone(),
            text: Some(BoundedText::new(prior_text).unwrap()),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: Usage::default(),
            finish_reason: ModelFinishReason::Stop,
            created_at: timestamp(),
        }),
        ConversationEntry::TurnTerminal(TurnTerminalEntry {
            seq: ConversationSeq::new(3),
            turn_id: prior,
            terminal: TurnTerminal::Completed,
            usage: Usage::default(),
            created_at: timestamp(),
        }),
        ConversationEntry::UserMessage(UserMessageEntry {
            seq: ConversationSeq::new(4),
            turn_id: turn_id(),
            input: UserInputRecord::new(BoundedText::new("current question").unwrap()).unwrap(),
            execution: TurnExecutionRecord::new(
                spec.model.clone(),
                spec.reasoning,
                effective_max_tool_rounds,
            )
            .unwrap(),
            created_at: timestamp(),
        }),
    ];
    ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
        .unwrap()
}

pub(super) fn ack_summary(
    conversation: &ConversationView,
    snapshot_head: ConversationSeq,
    draft: &SummaryDraft,
    spec: &SessionSpec,
) -> Result<CommittedUpdate, RunnerCommitError> {
    if conversation.head() != snapshot_head {
        return Err(RunnerCommitError::Stale);
    }
    let head = snapshot_head.next().ok_or(RunnerCommitError::Stale)?;
    let entry = ConversationEntry::Summary(SummaryEntry {
        seq: head,
        through: draft.through,
        summary: draft.summary.clone(),
        created_at: timestamp(),
    });
    let mut entries = conversation.entries().to_vec();
    entries.push(entry.clone());
    let conversation =
        ConversationView::from_validated_entries(spec, &SemanticLimits::default(), entries.into())
            .map_err(|_| RunnerCommitError::Stale)?;
    Ok(CommittedUpdate {
        previous_head: snapshot_head,
        entry,
        conversation,
    })
}

pub(super) fn request_with_compaction_kernel(
    spec: SessionSpec,
    bindings: SessionBindings,
    conversation: ConversationView,
    kernel: KernelConfig,
    cancellation: CancellationToken,
    turn_after: Duration,
    critical_capacity: usize,
) -> (
    TurnRunnerRequest,
    mpsc::Receiver<RunnerEvent>,
    mpsc::Receiver<RunnerProgress>,
) {
    let (critical_tx, critical_rx) = mpsc::channel(critical_capacity);
    let (progress_tx, progress_rx) = mpsc::channel(64);
    let environment = SessionEnvironment::build(&kernel, &spec, &bindings).unwrap();
    let request = TurnRunnerRequest::new(
        TurnRunnerIdentity {
            session_id: session_id(),
            instance_id: instance_id(),
            turn_id: turn_id(),
        },
        environment,
        4,
        conversation,
        TurnRunnerControl {
            cancellation,
            deadline: Instant::now() + turn_after,
            critical_tx,
            progress_tx,
        },
    )
    .unwrap();
    (request, critical_rx, progress_rx)
}
